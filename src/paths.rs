//! Runtime directory, socket paths, and filesystem helpers.
//!
//! # Why every session path is short
//!
//! `sun_path` is 104 bytes on macOS and 108 on Linux. Past that, Neovim
//! (libuv's `uv_pipe_bind2`) **silently truncates** and succeeds, while Rust
//! refuses the path, so nvim binds a path nvmux can never reach, with no
//! error. Hence [`MAX_SOCK_PATH`], checked before spawning. The std error has
//! `raw_os_error() == None`, so `ENAMETOOLONG` cannot be detected afterwards.

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nix::sys::stat::Mode;

use crate::error::PathError;

/// The runtime directory is always `/tmp/nvmux-<uid>`.
///
/// Not `$XDG_RUNTIME_DIR`: systemd deletes it at logout unless lingering is
/// enabled, killing detached sessions. Not `$TMPDIR`: on macOS it differs
/// between login, sudo and launchd contexts.
pub const RUNTIME_DIR_PREFIX: &str = "/tmp/nvmux-";

/// Maximum socket path length in **bytes** (not `char`s, which differ from
/// what the kernel measures on non-ASCII paths).
///
/// The hard limit is 103; the longest path nvmux composes is 44 bytes.
pub const MAX_SOCK_PATH: usize = 100;

/// Warn above this so budget creep shows up in tests first.
const WARN_SOCK_PATH: usize = 90;

/// The runtime directory for this user. Does not touch the filesystem.
pub fn runtime_dir() -> PathBuf {
    // `geteuid`: the directory we can actually own.
    PathBuf::from(format!("{}{}", RUNTIME_DIR_PREFIX, nix::unistd::geteuid()))
}

/// Resolve, create if needed, and security-check the runtime directory.
///
/// Run at startup and when a transport is built, not per operation: every
/// script that uses it repeats the check (`require_private_dir` in
/// `scripts/_prelude.sh`).
pub fn ensure_runtime_dir() -> Result<PathBuf, PathError> {
    let dir = runtime_dir();
    ensure_dir_secure(&dir)?;
    Ok(dir)
}

/// Security check on the **final component only**: on macOS `/tmp` is a
/// symlink to `/private/tmp`.
///
/// A code-execution control: `/tmp`'s sticky bit stops another user deleting
/// our directory but not creating it first, and a socket they can reach runs
/// `nvim_command("!sh")` as us.
pub(crate) fn ensure_dir_secure(dir: &Path) -> Result<(), PathError> {
    let io = |error| PathError::Io {
        path: dir.to_path_buf(),
        error,
    };

    // Bounded: a create loops once to re-check; more only in a create/remove
    // race with another nvmux.
    for _ in 0..8 {
        // `lstat`, not `stat`: a symlink to a directory passes `is_dir()`
        // through `stat` (the classic /tmp symlink attack).
        let meta = match std::fs::symlink_metadata(dir) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match DirBuilder::new().mode(0o700).create(dir) {
                    Ok(()) => {
                        // `mkdir`'s mode is masked by umask (`umask 0700` gives
                        // 0000). Safe to force: we just created it.
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
            // Never chmod a directory we did not create; refuse it instead.
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

/// The umask nvmux was started under, recorded by [`restrict_umask`].
static LAUNCH_UMASK: OnceLock<Mode> = OnceLock::new();

/// Set this process's umask to 077, recording the one it replaced. The first
/// call's record wins.
///
/// This covers only files nvmux writes itself (config, session metadata).
/// Spawned editors get the original mask back (see [`launch_umask`],
/// `proc::sh_command`, `pty::spawn_client_with`) so they create the user's files
/// as the shell would. The listen socket is protected not by the umask but by
/// the 0700 runtime directory ([`ensure_dir_secure`], also checked by
/// `scripts/spawn.sh` before every spawn) and an explicit `chmod 600`.
pub fn restrict_umask() {
    let previous = nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
    let _ = LAUNCH_UMASK.set(previous);
}

/// The umask to give the user's editor. `None` if [`restrict_umask`] never
/// ran (only `main` calls it), so children already inherit the right mask.
pub fn launch_umask() -> Option<Mode> {
    LAUNCH_UMASK.get().copied()
}

/// Validate a composed socket path against the `sun_path` budget.
pub fn check_sock_path(path: &Path) -> Result<(), PathError> {
    use std::os::unix::ffi::OsStrExt;
    let len = path.as_os_str().as_bytes().len();
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

/// Paths belonging to one session. Deliberately no rename; see
/// [`crate::transport::Transport::rename_session`].
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub sock: PathBuf,
    pub json: PathBuf,
    pub log: PathBuf,
}

impl SessionPaths {
    /// Build the paths for `id` under `dir`, rejecting a malformed id.
    ///
    /// These paths are unlinked and passed to a kill script, so an id like
    /// `../../x` must never get through; every path goes through here.
    pub fn new(dir: &Path, id: &str) -> Result<Self, PathError> {
        if !crate::ids::is_valid_id(id) {
            return Err(PathError::MalformedId(id.to_string()));
        }
        let sock = dir.join(format!("{id}.sock"));
        // `.json` and `.log` are bounded by PATH_MAX, not sun_path.
        check_sock_path(&sock)?;
        Ok(Self {
            json: dir.join(format!("{id}.json")),
            log: dir.join(format!("{id}.log")),
            sock,
        })
    }
}

/// The local end of an SSH-forwarded session socket, namespaced by host token
/// so equal session ids on two hosts do not collide.
pub fn forwarded_sock(dir: &Path, host_token: &str, id: &str) -> Result<PathBuf, PathError> {
    let p = dir.join(format!("{host_token}-{id}.sock"));
    check_sock_path(&p)?;
    Ok(p)
}

/// The SSH `ControlPath` for a host.
///
/// Computed here rather than with ssh's `%C`/`%h%p%r` tokens, whose expanded
/// length is unpredictable, so it stays within the budget. The longer
/// [`master_socket`] name is what gets checked.
pub fn control_path(dir: &Path, host_token: &str) -> Result<PathBuf, PathError> {
    let p = dir.join(format!("cm-{host_token}"));
    check_sock_path(&master_socket(&p, &"x".repeat(crate::ids::ID_LEN)))?;
    Ok(p)
}

/// The socket one master binds (unique per `nonce`) before it is linked to
/// the `ControlPath`; see `Ssh::ensure_master_within` for why a master never
/// binds the shared name.
pub fn master_socket(control_path: &Path, nonce: &str) -> PathBuf {
    let mut name = control_path.as_os_str().to_owned();
    name.push(".");
    name.push(nonce);
    PathBuf::from(name)
}

/// nvmux's own log file, one per user.
pub fn client_log(dir: &Path) -> PathBuf {
    dir.join("nvmux.log")
}

/// Write `contents` to `path` atomically via a synced sibling temp file and
/// `rename(2)`. The temp file is removed on failure.
///
/// The temp file is `<path>.tmp<pid>`, so a glob on the extension does not
/// match it.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_atomic_as(path, contents, None)
}

/// [`write_atomic`] with an optional `mode`, set on the temp file before the
/// rename.
fn write_atomic_as(path: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension(match path.extension() {
        Some(ext) => format!("{}.tmp{}", ext.to_string_lossy(), std::process::id()),
        None => format!("tmp{}", std::process::id()),
    });
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        if let Some(mode) = mode {
            // `fchmod` ignores the umask.
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Atomically rewrite the file `path` resolves to through symlinks, keeping
/// its mode. Used for the config: renaming over `path` would replace a
/// dotfiles symlink with a plain file and apply nvmux's 077 umask. A missing
/// file is created at the end of the links.
pub(crate) fn write_through(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let target = follow_links(path)?;
    let mode = match std::fs::metadata(&target) {
        Ok(meta) => Some(meta.permissions().mode() & 0o7777),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    write_atomic_as(&target, contents, mode)
}

/// Follow the last component's symlinks, even to a target that does not
/// exist yet.
fn follow_links(path: &Path) -> std::io::Result<PathBuf> {
    // Linux's limit.
    const MAX_LINKS: usize = 40;
    let mut at = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let to = std::fs::read_link(&at)?;
                at = match at.parent() {
                    Some(dir) if to.is_relative() => dir.join(to),
                    _ => to,
                };
            }
            _ => return Ok(at),
        }
    }
    Err(std::io::Error::other("too many levels of symbolic links"))
}

/// Create `path`'s parent directories as 0700 if missing, for the config and
/// state files. Not [`ensure_dir_secure`], which would reject an existing
/// 0755 `~/.config` and does not create parents.
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

    /// The umask is process-wide, so tests that change it, or write files that
    /// a hostile mask would leave unreadable, take turns.
    fn umask_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// One test, not three: the recorded mask is process-wide and set once.
    #[test]
    fn restricting_the_umask_keeps_the_mask_it_replaced() {
        let _guard = umask_lock();
        let original = nix::sys::stat::umask(Mode::from_bits_truncate(0o027));

        restrict_umask();

        // The only way to read the mask is to set it.
        let clamped = nix::sys::stat::umask(Mode::from_bits_truncate(0o077));
        assert_eq!(clamped.bits(), 0o077, "the process mask was not clamped");
        assert_eq!(
            launch_umask(),
            Some(Mode::from_bits_truncate(0o027)),
            "the mask the clamp replaced must survive it"
        );

        // A second call must not overwrite the launch mask.
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
        // A nested `nvmux/` would cost 6 bytes of sun_path for nothing.
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
            // The longest 32-bit uid.
            PathBuf::from("/tmp/nvmux-4294967295/6iger7ax-abcdefgh.sock"),
            PathBuf::from("/tmp/nvmux-4294967295/cm-6iger7ax"),
            // A master's socket, and ssh's temporary name for it (`.` plus
            // 16 random characters) before it is linked into place.
            master_socket(Path::new("/tmp/nvmux-4294967295/cm-6iger7ax"), "abcdefgh"),
            PathBuf::from("/tmp/nvmux-4294967295/cm-6iger7ax.abcdefgh.0123456789abcdef"),
        ];
        for c in cases {
            let len = c.as_os_str().as_bytes().len();
            assert!(len <= MAX_SOCK_PATH, "{} is {len} bytes", c.display());
            check_sock_path(&c).expect("should be within budget");
        }
    }

    #[test]
    fn every_master_binds_a_name_of_its_own() {
        let ctl = Path::new("/tmp/nvmux-501/cm-abcdefgh");
        let a = master_socket(ctl, "aaaaaaaa");
        assert_eq!(a, Path::new("/tmp/nvmux-501/cm-abcdefgh.aaaaaaaa"));
        assert_ne!(a, master_socket(ctl, "bbbbbbbb"));
        assert_eq!(a.parent(), ctl.parent());
    }

    /// The budget applies to the longer master socket name.
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

    /// A char-based check would pass this and fail at `bind()`.
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

    /// Entries whose names contain `tmp`.
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
        let _guard = umask_lock();
        let dir = crate::test_support::scratch_dir("paths-atomic");
        let path = dir.join("x.json");
        write_atomic(&path, b"{\"a\":1}\n").expect("write");
        assert_eq!(std::fs::read(&path).expect("read back"), b"{\"a\":1}\n");
        // Again, so the rename replaces a file.
        write_atomic(&path, b"{\"a\":2}\n").expect("rewrite");
        assert_eq!(std::fs::read(&path).expect("read back"), b"{\"a\":2}\n");
        assert!(strays(&dir).is_empty(), "left temp files behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory at the target makes the rename fail.
    #[test]
    fn a_failed_atomic_write_cleans_up_after_itself() {
        let _guard = umask_lock();
        let dir = crate::test_support::scratch_dir("paths-atomic-fail");
        let path = dir.join("x.json");
        std::fs::create_dir(&path).expect("a directory in the way");
        assert!(write_atomic(&path, b"{}").is_err());
        assert!(strays(&dir).is_empty(), "left temp files behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_through_a_link_keeps_the_link_and_the_mode() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = umask_lock();
        let dir = crate::test_support::scratch_dir("paths-through");
        let repo = dir.join("dotfiles");
        std::fs::create_dir(&repo).expect("mkdir");
        let real = repo.join("config.toml");
        std::fs::write(&real, "old").expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink("dotfiles/config.toml", &link).expect("link");

        write_through(&link, b"new").expect("write");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("stat")
                .file_type()
                .is_symlink(),
            "the link was replaced"
        );
        assert_eq!(std::fs::read_to_string(&real).expect("read"), "new");
        let mode = std::fs::metadata(&real).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "the mode changed");
        assert!(strays(&repo).is_empty(), "left temp files behind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_through_to_nothing_makes_the_file() {
        let _guard = umask_lock();
        let dir = crate::test_support::scratch_dir("paths-through-new");
        let plain = dir.join("a.toml");
        write_through(&plain, b"a").expect("write");
        assert_eq!(std::fs::read_to_string(&plain).expect("read"), "a");

        let link = dir.join("b.toml");
        std::os::unix::fs::symlink("elsewhere.toml", &link).expect("link");
        write_through(&link, b"b").expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.join("elsewhere.toml")).expect("read"),
            "b"
        );
        assert!(std::fs::symlink_metadata(&link)
            .expect("stat")
            .file_type()
            .is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each test constructs the attack the check must stop.
    mod security {
        use super::*;

        /// A scratch path that does not exist yet.
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

        /// The /tmp symlink attack: only lstat sees the link.
        #[test]
        fn rejects_a_symlink_to_a_directory() {
            let real = scratch("symlink-target");
            let link = scratch("symlink");
            std::fs::create_dir_all(&real).expect("mkdir");
            std::os::unix::fs::symlink(&real, &link).expect("symlink");

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

        /// Set when [`survives_a_hostile_umask`] reruns the test binary for
        /// [`hostile_umask_child`].
        const HOSTILE_CHILD: &str = "NVMUX_TEST_HOSTILE_UMASK_CHILD";

        /// Runs in a child process: the umask is process-wide and
        /// `umask_lock()` only guards this module's tests, so a hostile mask
        /// would leak into files and shells of other tests in this binary.
        #[test]
        fn survives_a_hostile_umask() {
            let out = std::process::Command::new(std::env::current_exe().expect("the tests"))
                .args([
                    "--exact",
                    "paths::tests::security::hostile_umask_child",
                    "--ignored",
                    "--test-threads=1",
                ])
                .env(HOSTILE_CHILD, "1")
                .output()
                .expect("run the child");
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success(), "the child failed: {said}");
            assert!(said.contains("1 passed"), "the child did not run: {said}");
        }

        /// The child half of [`survives_a_hostile_umask`]; a no-op without
        /// `HOSTILE_CHILD`, so a stray `--ignored` run is harmless.
        #[test]
        #[ignore = "the child half of survives_a_hostile_umask; run by it, not by hand"]
        fn hostile_umask_child() {
            if std::env::var_os(HOSTILE_CHILD).is_none() {
                return;
            }
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
