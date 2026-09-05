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

use std::fs::DirBuilder;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

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
/// Call this before every operation that touches the directory: it is cheap, and
/// `/tmp` reapers do delete idle directories.
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

/// Restrict the file mode creation mask for this process, before spawning any
/// nvim. Neovim creates its listen socket with `0777 & ~umask`; the 0700
/// directory is the primary control, but closing the hole at both levels costs
/// nothing.
pub fn restrict_umask() {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
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

/// The SSH `ControlPath` for a host.
///
/// Computed by us rather than left to ssh's `%C`/`%h%p%r` tokens, which expand
/// to unpredictable lengths. ssh enforces its own limit and at least fails
/// loudly (`ControlPath too long ('...' >= 108 bytes)`), but a path we choose is
/// a path we can keep inside the budget.
pub fn control_path(dir: &Path, host_token: &str) -> Result<PathBuf, PathError> {
    let p = dir.join(format!("cm-{host_token}"));
    check_sock_path(&p)?;
    Ok(p)
}

/// nvmux's own log file. One per user, not per session.
pub fn client_log(dir: &Path) -> PathBuf {
    dir.join("nvmux.log")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        ];
        for c in cases {
            let len = c.as_os_str().as_bytes().len();
            assert!(len <= MAX_SOCK_PATH, "{} is {len} bytes", c.display());
            check_sock_path(&c).expect("should be within budget");
        }
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

    /// The checks below are security controls, so they are tested as such:
    /// each one constructs the attack it is supposed to stop.
    mod security {
        use super::*;

        fn scratch(tag: &str) -> PathBuf {
            let p = std::env::temp_dir().join(format!("nvmux-sec-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            let _ = std::fs::remove_file(&p);
            p
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
