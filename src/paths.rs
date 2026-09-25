//! Runtime directory resolution and socket path construction.
//!
//! # Why every session path is short
//!
//! `sun_path` is 104 bytes on macOS and 108 on Linux, and the two halves of
//! nvmux disagree about what happens past that. Neovim (through libuv)
//! **silently truncates** — `uv_pipe_bind2` does `if (namelen >
//! sizeof(saddr.up.path)) namelen = sizeof(saddr.up.path);` and exits 0 — while
//! Rust refuses the same path outright. The result is a healthy nvim bound to a
//! truncated path that nvmux can never reach, with no error printed anywhere.
//! Hence [`MAX_SOCK_PATH`], checked before spawning.
//!
//! The std error carries `raw_os_error() == None`, so there is no
//! `ENAMETOOLONG` to detect after the fact.
//!
//! The one atomic file write nvmux makes ([`write_atomic`]) and the gentle
//! parent-directory creation the config and state files share
//! ([`create_private_parent`]) live here too, with the rest of the filesystem
//! hygiene.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nix::sys::stat::Mode;

use crate::error::PathError;

/// The runtime directory is always `/tmp/nvmux-<uid>`, on both platforms.
///
/// `$XDG_RUNTIME_DIR` is deliberately **not** consulted: on Linux systemd
/// destroys it at logout unless `loginctl enable-linger` is set, so detached
/// sessions would silently die — precisely what nvmux exists to prevent. Nor is
/// `$TMPDIR`, which on macOS differs between login, sudo and launchd contexts.
pub const RUNTIME_DIR_PREFIX: &str = "/tmp/nvmux-";

/// Reject socket paths longer than this, in **bytes**.
///
/// The hard limit is 103 (see the module docs); 100 keeps a free margin, since
/// the longest path nvmux composes is 44 bytes. Counted in bytes, not `char`s:
/// on a non-ASCII path `chars().count()` diverges from what the kernel measures.
pub const MAX_SOCK_PATH: usize = 100;

/// Log a warning above this, so budget creep shows up in tests before it
/// becomes a hard failure in someone's home directory.
const WARN_SOCK_PATH: usize = 90;

/// The runtime directory for this user. Does not touch the filesystem.
pub fn runtime_dir() -> PathBuf {
    // `geteuid` rather than `getuid`: the directory we can actually own.
    PathBuf::from(format!("{}{}", RUNTIME_DIR_PREFIX, nix::unistd::geteuid()))
}

/// Resolve, create if needed, and security-check the runtime directory.
///
/// Run once at startup (`main`) and again when a transport is built. It is not
/// repeated per operation: `/tmp` reapers do delete idle directories, but the
/// only moment that matters for a socket is its creation, and
/// `scripts/spawn.sh` repeats this same check itself before every spawn.
pub fn ensure_runtime_dir() -> Result<PathBuf, PathError> {
    let dir = runtime_dir();
    ensure_dir_secure(&dir)?;
    Ok(dir)
}

/// The security check, run on the **final component only** — on macOS `/tmp` is
/// itself a symlink to `/private/tmp`, so rejecting any symlinked component
/// would refuse to start on every Mac.
///
/// A code-execution control, not a privacy nicety: `/tmp` is mode 1777, so its
/// sticky bit stops another user *deleting* our directory but not *creating* it
/// first, and a socket they can reach runs `nvim_command("!sh")` as us.
pub(crate) fn ensure_dir_secure(dir: &Path) -> Result<(), PathError> {
    let io = |source| PathError::Io {
        path: dir.to_path_buf(),
        source,
    };

    // Bounded retry: the only way round the loop is losing a create/remove race
    // with another nvmux.
    for _ in 0..8 {
        // `symlink_metadata`, never `metadata`: a symlink *to* a directory
        // satisfies `create_dir_all` and reports `is_dir() == true` through
        // `stat`. Only `lstat` reveals it, and that is the exact shape of the
        // classic /tmp symlink attack.
        let meta = match std::fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(dir) {
                    Ok(()) => {
                        // `mkdir`'s mode is masked by umask: under a hostile
                        // `umask 0700` it yields mode 0000 and every later open
                        // fails with EACCES. Safe to force — we just created it.
                        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                            .map_err(io)?;
                        continue;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(io(e)),
                }
            }
            Err(e) => return Err(io(e)),
        };

        if !meta.is_dir() {
            return Err(PathError::NotADirectory(dir.to_path_buf()));
        }
        let euid = nix::unistd::geteuid().as_raw();
        if meta.uid() != euid {
            return Err(PathError::BadOwner {
                path: dir.to_path_buf(),
                owner: meta.uid(),
                expected: euid,
            });
        }
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            // Never chmod a directory we did not create: silently tightening
            // someone else's directory is worse than refusing to use it.
            return Err(PathError::BadMode {
                path: dir.to_path_buf(),
                mode,
            });
        }
        return Ok(());
    }

    Err(io(std::io::Error::other(
        "runtime directory kept changing underneath us",
    )))
}

/// The umask nvmux was started under, kept by [`restrict_umask`] so that what
/// it clamps can be handed back to the editor. See [`launch_umask`].
static LAUNCH_UMASK: OnceLock<Mode> = OnceLock::new();

/// Restrict the file mode creation mask for **this process**, remembering the
/// mask it replaced.
///
/// This covers the files nvmux writes itself — its config file, a local
/// session's metadata — and stops there. The mask is deliberately *not* left
/// on the session: a session's editor writes the user's files, and an editor
/// that creates them more privately than the shell it was started from is an
/// editor that behaves differently for no reason the user can see. So the
/// recorded mask is given back to what nvmux spawns; see [`launch_umask`],
/// `proc::sh_command` and `pty::client_command`.
///
/// What that leaves protecting the listen socket, which is created with
/// `0777 & ~umask` and is what this clamp was reached for: a runtime directory
/// that is 0700, owned by us, checked by [`ensure_dir_secure`] at startup and
/// when a transport is built, and by `scripts/spawn.sh` before every spawn —
/// the one moment a socket is created — so a socket is out of reach for as
/// long as it exists, whatever mode it is born with — and
/// an explicit `chmod 600` on the socket once it appears. Both are controls
/// that hold on their own; the umask never was one.
///
/// The first call wins, so the mask recorded is the one nvmux was started
/// with rather than one a later call observed.
pub fn restrict_umask() {
    let previous = nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
    let _ = LAUNCH_UMASK.set(previous);
}

/// The umask nvmux was started with, for handing to a process that is the
/// user's editor rather than nvmux's own bookkeeping.
///
/// `None` when [`restrict_umask`] has not run — only `main` calls it — in
/// which case this process never moved the mask and there is nothing to
/// restore: a child inherits the right one by doing nothing.
pub fn launch_umask() -> Option<Mode> {
    LAUNCH_UMASK.get().copied()
}

/// Validate a composed socket path against the `sun_path` budget.
///
/// The bytes are the platform's own encoding of the path, which for a path on
/// a Unix host — a remote session's included, whatever this machine is — is
/// what its kernel measures.
pub fn check_sock_path(path: &Path) -> Result<(), PathError> {
    let len = path.as_os_str().as_encoded_bytes().len();
    if len > MAX_SOCK_PATH {
        return Err(PathError::TooLong {
            path: path.to_path_buf(),
            len,
            max: MAX_SOCK_PATH,
        });
    }
    if len > WARN_SOCK_PATH {
        tracing::warn!(
            path = %path.display(),
            len,
            max = MAX_SOCK_PATH,
            "socket path is close to the sun_path limit"
        );
    }
    Ok(())
}

/// Paths belonging to one session, in one directory. There is deliberately no
/// rename here — see [`crate::transport::Transport::rename_session`].
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub sock: PathBuf,
    pub json: PathBuf,
    pub log: PathBuf,
}

impl SessionPaths {
    /// Build the three paths for `id` under `dir`, rejecting a malformed id.
    ///
    /// These paths are unlinked, connected to and handed to a kill script, so
    /// `../../elsewhere/precious` would take all three outside the runtime
    /// directory. The listing already drops malformed ids; this is the choke
    /// point every path goes through.
    pub fn new(dir: &Path, id: &str) -> Result<Self, PathError> {
        if !crate::ids::is_valid_id(id) {
            return Err(PathError::MalformedId(id.to_string()));
        }
        let sock = dir.join(format!("{id}.sock"));
        // Only the socket is length-checked. `.json` and `.log` are opened by
        // path, which is bounded by PATH_MAX (1024+), not by sun_path.
        check_sock_path(&sock)?;
        Ok(Self {
            json: dir.join(format!("{id}.json")),
            log: dir.join(format!("{id}.log")),
            sock,
        })
    }
}

/// The local end of an SSH-forwarded session socket.
///
/// Namespaced by host token so that two hosts each holding a session with the
/// same id cannot collide on one local path.
pub fn forwarded_sock(dir: &Path, host_token: &str, id: &str) -> Result<PathBuf, PathError> {
    let p = dir.join(format!("{host_token}-{id}.sock"));
    check_sock_path(&p)?;
    Ok(p)
}

/// The SSH `ControlPath` for a host: the name every ssh command nvmux runs
/// there finds the host's master connection by.
///
/// Computed by us rather than left to ssh's `%C`/`%h%p%r` tokens, which expand
/// to unpredictable lengths. ssh enforces its own limit and at least fails
/// loudly (`ControlPath too long ('...' >= 108 bytes)`), but a path we choose is
/// a path we can keep inside the budget.
///
/// A master binds a longer name than this one — [`master_socket`] — so that
/// is the one checked.
pub fn control_path(dir: &Path, host_token: &str) -> Result<PathBuf, PathError> {
    let p = dir.join(format!("cm-{host_token}"));
    check_sock_path(&master_socket(&p, &"x".repeat(crate::ids::ID_LEN)))?;
    Ok(p)
}

/// The socket one master connection binds, named for that master alone and
/// then linked to the `ControlPath` — see `Ssh::ensure_master_within` for why
/// a master must never bind the shared name itself.
///
/// `nonce` is what makes it that master's: a fresh one per master started.
pub fn master_socket(control_path: &Path, nonce: &str) -> PathBuf {
    let mut name = control_path.as_os_str().to_owned();
    name.push(".");
    name.push(nonce);
    PathBuf::from(name)
}

/// nvmux's own log file. One per user, not per session.
pub fn client_log(dir: &Path) -> PathBuf {
    dir.join("nvmux.log")
}

/// Write `contents` to `path` atomically: a sibling temp file, synced, then
/// `rename(2)`, which within one directory is atomic — so a concurrent reader
/// sees the old contents or the new and never a half-written file. The temp
/// file is removed again if any step fails.
///
/// The temp name keeps the target's extension in front of `.tmp<pid>`, so a
/// glob on the extension does not pick the temp file up as a real one.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension(match path.extension() {
        Some(ext) => format!("{}.tmp{}", ext.to_string_lossy(), std::process::id()),
        None => format!("tmp{}", std::process::id()),
    });
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Create `path`'s parent directory, private to the user, if it is missing.
///
/// A gentle `DirBuilder`, not [`ensure_dir_secure`]: that is `/tmp` hardening
/// that would reject a pre-existing `~/.config` at `0755` and does not create
/// missing parents. For the files nvmux writes under the user's own
/// directories — the config and the state file.
pub(crate) fn create_private_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The process umask is one value shared by every thread, so the tests that
    /// move it take turns. Without this, one test's hostile mask is another's
    /// launch mask, and the shells the rest of this binary starts inherit it.
    fn umask_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `restrict_umask` is a trade, and both halves matter: the process is
    /// clamped, *and* the mask that clamp replaced is kept, because that is the
    /// one a session's editor is given back.
    ///
    /// One test for the whole story rather than three: the recorded mask is
    /// process-wide and written once, so a second test could only ever observe
    /// what this one left behind.
    #[test]
    fn restricting_the_umask_keeps_the_mask_it_replaced() {
        let _guard = umask_lock();
        let original = nix::sys::stat::umask(Mode::from_bits_truncate(0o027));

        restrict_umask();

        // Read the mask back the only way there is — by setting it — and put
        // the value straight back.
        let clamped = nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
        assert_eq!(clamped.bits(), 0o077, "the process mask was not clamped");
        assert_eq!(
            launch_umask(),
            Some(Mode::from_bits_truncate(0o027)),
            "the mask the clamp replaced must survive it"
        );

        // A second call must not overwrite the launch mask with whatever the
        // process happens to be running under by the time it lands.
        nix::sys::stat::umask(Mode::from_bits_truncate(0o007));
        restrict_umask();
        assert_eq!(
            launch_umask(),
            Some(Mode::from_bits_truncate(0o027)),
            "the first call must win"
        );

        nix::sys::stat::umask(original);
    }

    #[test]
    fn runtime_dir_has_no_doubled_segment() {
        let d = runtime_dir();
        let s = d.to_string_lossy();
        assert!(s.starts_with("/tmp/nvmux-"), "unexpected prefix: {s}");
        // `/tmp/nvmux-<uid>` is already private to us, so a nested `nvmux/`
        // would cost 6 bytes of the sun_path budget for no isolation.
        assert_eq!(
            s.matches("nvmux").count(),
            1,
            "doubled nvmux segment in {s}"
        );
    }

    #[test]
    fn session_paths_share_a_stem() {
        let p = SessionPaths::new(Path::new("/tmp/nvmux-501"), "abcdefgh").expect("within budget");
        assert_eq!(p.sock, Path::new("/tmp/nvmux-501/abcdefgh.sock"));
        assert_eq!(p.json, Path::new("/tmp/nvmux-501/abcdefgh.json"));
        assert_eq!(p.log, Path::new("/tmp/nvmux-501/abcdefgh.log"));
    }

    #[test]
    fn realistic_paths_have_ample_headroom() {
        use std::os::unix::ffi::OsStrExt;
        let cases = [
            PathBuf::from("/tmp/nvmux-501/abcdefgh.sock"),
            PathBuf::from("/tmp/nvmux-1000/abcdefgh.sock"),
            // The worst uid a 32-bit uid_t can produce.
            PathBuf::from("/tmp/nvmux-4294967295/6iger7ax-abcdefgh.sock"),
            PathBuf::from("/tmp/nvmux-4294967295/cm-6iger7ax"),
            // A master's own socket, and the name ssh binds it under before
            // linking it into place: `.` and sixteen random characters more.
            master_socket(Path::new("/tmp/nvmux-4294967295/cm-6iger7ax"), "abcdefgh"),
            PathBuf::from("/tmp/nvmux-4294967295/cm-6iger7ax.abcdefgh.0123456789abcdef"),
        ];
        for c in cases {
            let len = c.as_os_str().as_bytes().len();
            assert!(len <= MAX_SOCK_PATH, "{} is {len} bytes", c.display());
            check_sock_path(&c).expect("should be within budget");
        }
    }

    /// Every master for a host binds a name of its own, beside the
    /// `ControlPath` it is then linked to.
    #[test]
    fn every_master_binds_a_name_of_its_own() {
        let ctl = Path::new("/tmp/nvmux-501/cm-abcdefgh");
        let a = master_socket(ctl, "aaaaaaaa");
        assert_eq!(a, Path::new("/tmp/nvmux-501/cm-abcdefgh.aaaaaaaa"));
        assert_ne!(a, master_socket(ctl, "bbbbbbbb"));
        assert_eq!(a.parent(), ctl.parent());
    }

    /// The name a master binds is the longer of the two, so it is the one the
    /// budget is held to: a `ControlPath` that would fit on its own is refused
    /// when its masters' sockets would not.
    #[test]
    fn a_control_path_is_refused_when_its_masters_sockets_would_not_fit() {
        let dir = PathBuf::from(format!("/tmp/{}", "a".repeat(80)));
        assert!(
            check_sock_path(&dir.join("cm-abcdefgh")).is_ok(),
            "precondition: the ControlPath alone is within budget"
        );
        assert!(matches!(
            control_path(&dir, "abcdefgh"),
            Err(PathError::TooLong { .. })
        ));
    }

    #[test]
    fn overlong_paths_are_rejected() {
        let long = PathBuf::from(format!("/tmp/{}/x.sock", "a".repeat(120)));
        let err = check_sock_path(&long).expect_err("should exceed the budget");
        match err {
            PathError::TooLong { len, max, .. } => {
                assert!(len > max);
                assert_eq!(max, MAX_SOCK_PATH);
            }
            other => panic!("expected TooLong, got {other:?}"),
        }
    }

    /// Measured in bytes, because that is what the kernel copies into
    /// `sun_path`. A char-based check would pass this and fail at `bind()`.
    #[test]
    fn budget_is_counted_in_bytes_not_chars() {
        use std::os::unix::ffi::OsStrExt;
        // 51 two-byte characters: 102 bytes, 51 chars.
        let dir: String = "é".repeat(51);
        let p = PathBuf::from(format!("/{dir}"));
        let bytes = p.as_os_str().as_bytes().len();
        let chars = p.to_string_lossy().chars().count();
        assert!(
            bytes > MAX_SOCK_PATH && chars < MAX_SOCK_PATH,
            "test vector is not discriminating: {bytes} bytes, {chars} chars"
        );
        assert!(
            check_sock_path(&p).is_err(),
            "byte length must be what is measured"
        );
    }

    #[test]
    fn forwarded_socket_is_namespaced_by_host() {
        let dir = Path::new("/tmp/nvmux-501");
        let a = forwarded_sock(dir, "aaaaaaaa", "abcdefgh").expect("ok");
        let b = forwarded_sock(dir, "bbbbbbbb", "abcdefgh").expect("ok");
        assert_ne!(
            a, b,
            "same session id on two hosts must not share a local path"
        );
    }

    /// The entries of a directory whose names carry `tmp`: what an atomic
    /// write must never leave behind.
    fn strays(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect()
    }

    #[test]
    fn an_atomic_write_lands_whole_and_leaves_no_temp_file() {
        let dir = crate::test_support::scratch_dir("paths-atomic");
        let path = dir.join("x.json");
        write_atomic(&path, b"{\"a\":1}\n").expect("write");
        assert_eq!(std::fs::read(&path).expect("read back"), b"{\"a\":1}\n");
        // And again over the top, which is the rename replacing a file.
        write_atomic(&path, b"{\"a\":2}\n").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read back"), b"{\"a\":2}\n");
        assert!(strays(&dir).is_empty(), "left temp files behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A rename that fails — a directory sits where the file should go — must
    /// not leave the temp file for a listing to find.
    #[test]
    fn a_failed_atomic_write_cleans_up_after_itself() {
        let dir = crate::test_support::scratch_dir("paths-atomic-fail");
        let path = dir.join("x.json");
        std::fs::create_dir(&path).expect("a directory in the way");
        assert!(write_atomic(&path, b"{}").is_err());
        assert!(strays(&dir).is_empty(), "left temp files behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The checks below are security controls, so they are tested as such:
    /// each one constructs the attack it is supposed to stop.
    mod security {
        use super::*;

        /// Absent, not created: every case here makes its own thing at the
        /// path, or wants `ensure_dir_secure` to.
        fn scratch(tag: &str) -> PathBuf {
            crate::test_support::scratch_path(&format!("sec-{tag}"))
        }

        #[test]
        fn creates_a_missing_directory_as_0700() {
            let d = scratch("create");
            ensure_dir_secure(&d).expect("should create");
            let mode = std::fs::symlink_metadata(&d).expect("stat").mode() & 0o777;
            assert_eq!(mode, 0o700, "created with mode {mode:04o}");
            std::fs::remove_dir_all(&d).ok();
        }

        #[test]
        fn accepts_a_directory_it_already_owns() {
            let d = scratch("idempotent");
            ensure_dir_secure(&d).expect("create");
            ensure_dir_secure(&d).expect("second call must be a no-op");
            std::fs::remove_dir_all(&d).ok();
        }

        /// The /tmp symlink attack: `create_dir_all` succeeds on a
        /// symlink-to-directory and `is_dir()` reports true; only lstat differs.
        #[test]
        fn rejects_a_symlink_to_a_directory() {
            let real = scratch("symlink-target");
            let link = scratch("symlink");
            std::fs::create_dir_all(&real).expect("mkdir");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");

            // Establish that the naive check would have been fooled.
            assert!(
                std::fs::metadata(&link).expect("stat").is_dir(),
                "precondition: stat() sees a directory"
            );

            let err = ensure_dir_secure(&link).expect_err("must reject a symlink");
            assert!(
                matches!(err, PathError::NotADirectory(_)),
                "want NotADirectory, got {err:?}"
            );
            std::fs::remove_file(&link).ok();
            std::fs::remove_dir_all(&real).ok();
        }

        #[test]
        fn rejects_a_group_or_world_accessible_directory() {
            for mode in [0o755, 0o770, 0o707, 0o701] {
                let d = scratch(&format!("mode{mode:o}"));
                std::fs::create_dir_all(&d).expect("mkdir");
                std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).expect("chmod");
                let err = match ensure_dir_secure(&d) {
                    Err(e) => e,
                    Ok(()) => panic!("must reject a directory with mode {mode:o}"),
                };
                assert!(
                    matches!(err, PathError::BadMode { .. }),
                    "want BadMode for {mode:o}, got {err:?}"
                );
                std::fs::remove_dir_all(&d).ok();
            }
        }

        #[test]
        fn rejects_a_regular_file_in_the_way() {
            let d = scratch("regular");
            std::fs::write(&d, b"not a directory").expect("write");
            let err = ensure_dir_secure(&d).expect_err("must reject a file");
            assert!(
                matches!(err, PathError::NotADirectory(_)),
                "want NotADirectory, got {err:?}"
            );
            std::fs::remove_file(&d).ok();
        }

        /// A hostile umask masks mkdir's mode argument, so the mode has to be
        /// forced afterwards.
        #[test]
        fn survives_a_hostile_umask() {
            let _guard = umask_lock();
            let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o700));
            let d = scratch("umask");
            let result = ensure_dir_secure(&d);
            let mode = std::fs::symlink_metadata(&d).map(|m| m.mode() & 0o777);
            nix::sys::stat::umask(previous);

            result.expect("must succeed under a hostile umask");
            assert_eq!(mode.expect("stat"), 0o700, "umask was allowed to win");
            std::fs::remove_dir_all(&d).ok();
        }
    }
}
