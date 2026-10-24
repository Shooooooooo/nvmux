//! Listing directories on the session host, for the create prompt's
//! completion.
//!
//! Separate from [`crate::transport::Transport`] because that is not `Send`
//! and completion runs on a worker thread (see [`crate::ui::complete`]). Each
//! transport hands out a [`DirSource`]: an owned `Send` handle to reach the
//! same host (over ssh, the host and the master's `ControlPath`, so the
//! worker's shell is a new channel on the existing connection).
//!
//! The worker's [`Lister`] keeps its own shell across questions, not the
//! transport's busy one; over ssh it is unattended, so it never sits on an
//! invisible prompt.
//!
//! Both arms run `scripts/dirs.sh` in a long-lived shell, so sort order, cap
//! and odd-name handling cannot drift between local and remote (as they would
//! with `read_dir` locally).

use std::path::PathBuf;

use crate::error::{NvmuxError, Result};
use crate::proc::Shell;
use crate::shell;
use crate::ssh::Ssh;
use crate::transport::protocol::{self, Listing};

/// A `Send` handle that can list directories on one session host. Cheap: the
/// ssh arm is a hostname and a path, not a connection.
#[derive(Debug, Clone)]
pub enum DirSource {
    Local,
    Ssh { host: String, control_path: PathBuf },
}

impl DirSource {
    /// A shell on this host for questions nobody is watching. Over ssh it is
    /// unattended: it never prompts and gives up on an unreachable host (see
    /// `ssh::unattended`).
    pub fn start_shell(&self) -> Result<Shell> {
        Ok(match self {
            DirSource::Local => Shell::local()?,
            DirSource::Ssh { host, control_path } => {
                Ssh::new(host.clone(), control_path.clone()).start_unattended_shell()?
            }
        })
    }
}

/// Directory listings from one host, through one shell kept across questions.
/// Owned by the completion worker. The shell starts on the first question or
/// [`Lister::warm`], and is replaced if it dies.
pub struct Lister {
    source: DirSource,
    shell: Option<Shell>,
}

impl Lister {
    pub fn new(source: DirSource) -> Self {
        Self {
            source,
            shell: None,
        }
    }

    /// Start the shell early, so its startup (over ssh, a login shell) is paid
    /// while the user types. Failure is not reported here; the first question
    /// retries and reports it.
    pub fn warm(&mut self) {
        let _ = self.shell();
    }

    fn shell(&mut self) -> Result<&mut Shell> {
        if self.shell.as_mut().is_none_or(|shell| !shell.is_alive()) {
            self.shell = Some(self.source.start_shell()?);
        }
        Ok(self.shell.as_mut().expect("just started"))
    }

    /// Every subdirectory of `dir`, dotted ones included (hidden later unless
    /// the query asks). Unfiltered: matching is fuzzy, in
    /// [`crate::ui::complete`], so one answer serves every keystroke in the
    /// directory. A missing directory is an empty listing, not an error: half
    /// a path is the prompt's normal state.
    pub fn children(&mut self, dir: &str) -> Result<Listing> {
        // The status is ignored: the worker shows an empty listing on error
        // anyway. A dead shell is dropped and replaced on the next question.
        let out = self
            .shell()?
            .run(shell::DIRS_SCRIPT, &[dir])
            .map_err(|died| {
                self.shell = None;
                NvmuxError::Io(std::io::Error::other(died))
            })?;
        protocol::parse_dirs(&out.stdout)
    }
}

/// Split a typed path into its inert and live parts.
///
/// `//` restarts from the root, so everything up to the last one is inert
/// (drawn dim): `/home/shu//etc` is `/etc`, with `/home/shu/` left behind.
///
/// Only the live half may reach the session host: POSIX reads `a//b` as `a/b`,
/// so `cd /home/shu//etc` would silently land in `/home/shu/etc`. The split
/// falls between the two slashes, so the live half stays absolute.
pub fn anchored(input: &str) -> (&str, &str) {
    match input.rfind("//") {
        Some(at) => input.split_at(at + 1),
        None => ("", input),
    }
}

/// Split what has been typed at the last `/` into the directory to list and
/// the name to match, so every keystroke within a directory reuses its
/// listing. `None` without a `/`: directories are absolute, so there is
/// nothing to list.
pub fn split(input: &str) -> Option<(&str, &str)> {
    let at = input.rfind('/')?;
    let (dir, rest) = input.split_at(at);
    // `/etc` splits into `/` and `etc`, not `""` and `etc`.
    let dir = if dir.is_empty() { "/" } else { dir };
    Some((dir, &rest[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_slash_starts_again_from_the_root() {
        assert_eq!(anchored("/home/shu//etc"), ("/home/shu/", "/etc"));
        assert_eq!(
            anchored("/home/shu/projects//etc"),
            ("/home/shu/projects/", "/etc")
        );
        // The last one wins.
        assert_eq!(anchored("/a//b//c"), ("/a//b/", "/c"));
        // A bare `//` is the root itself.
        assert_eq!(anchored("//"), ("/", "/"));
        // Three in a row: the split is at the last pair.
        assert_eq!(anchored("///etc"), ("//", "/etc"));
        assert_eq!(anchored("/home/shu/projects"), ("", "/home/shu/projects"));
        assert_eq!(anchored("/"), ("", "/"));
        assert_eq!(anchored(""), ("", ""));
        // A trailing `//` is still the root, mid-typing.
        assert_eq!(anchored("/home/shu//"), ("/home/shu/", "/"));
    }

    /// `validate_directory` only accepts absolute paths.
    #[test]
    fn what_a_double_slash_leaves_behind_is_still_absolute() {
        for input in ["/home/shu//etc", "//", "///x", "/a//b//c"] {
            let (_, live) = anchored(input);
            assert!(live.starts_with('/'), "{input:?} left {live:?}");
        }
    }

    #[test]
    fn a_path_is_split_into_the_directory_to_list_and_the_name_to_match() {
        assert_eq!(split("/home/you/pro"), Some(("/home/you", "pro")));
        assert_eq!(split("/home/you/"), Some(("/home/you", "")));
        assert_eq!(split("/etc"), Some(("/", "etc")));
        assert_eq!(split("/"), Some(("/", "")));
        // Nothing to list.
        assert_eq!(split("home"), None);
        assert_eq!(split(""), None);
    }

    /// A whitespace-splitting version would pass every other test here.
    #[test]
    fn a_directory_name_with_a_space_in_it_survives_the_split() {
        assert_eq!(split("/home/you/my pro"), Some(("/home/you", "my pro")));
    }

    fn local() -> Lister {
        Lister::new(DirSource::Local)
    }

    /// Also tests the embedded script's contract, which the remote arm shares.
    #[test]
    fn the_local_source_lists_every_child_including_the_dotted_ones() {
        let root = crate::test_support::scratch_path("dirs-listing");
        for name in ["alpha", "beta", "beta-two", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).expect("make a directory");
        }
        std::fs::write(root.join("a-file"), b"not a directory").expect("write a file");
        let dir = root.to_string_lossy().into_owned();

        let mut lister = local();
        let all = lister.children(&dir).expect("list");
        assert_eq!(
            all.names,
            ["alpha", "beta", "beta-two", ".hidden"],
            "every directory and no files; dotted ones last, and hidden further up"
        );
        assert!(!all.truncated);
        assert!(
            !all.names.iter().any(|n| n == "." || n == ".."),
            "`.` and `..` are never completions: {:?}",
            all.names
        );

        // Half-typed paths are the normal state of a prompt, not an error.
        let missing = lister
            .children(&root.join("nope").to_string_lossy())
            .expect("a directory that is not there is an empty answer");
        assert!(missing.names.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Names a shell would otherwise split on or treat as a pattern.
    #[test]
    fn odd_directory_names_survive_the_listing() {
        let root = crate::test_support::scratch_path("dirs-odd");
        for name in ["a b", "c*d", "c?d", "cxd"] {
            std::fs::create_dir_all(root.join(name)).expect("make a directory");
        }
        let dir = root.to_string_lossy().into_owned();

        let all = local().children(&dir).expect("list");
        assert_eq!(
            all.names,
            ["a b", "c*d", "c?d", "cxd"],
            "a name with a space is one name, and a `*` is a character"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_shell_serves_every_question_and_a_dead_one_is_replaced() {
        let root = crate::test_support::scratch_path("dirs-oneshell");
        std::fs::create_dir_all(root.join("only")).expect("make a directory");
        let dir = root.to_string_lossy().into_owned();

        let mut lister = local();
        lister.warm();
        let pid = lister.shell.as_ref().expect("warmed").pid();
        for _ in 0..3 {
            assert_eq!(lister.children(&dir).expect("list").names, ["only"]);
            assert_eq!(lister.shell.as_ref().expect("kept").pid(), pid);
        }

        // The shell goes away while idle — an expired master, over ssh.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        assert!(
            crate::test_support::wait_until(std::time::Duration::from_secs(2), || !lister
                .shell
                .as_mut()
                .expect("still held")
                .is_alive()),
            "the shell did not die"
        );

        assert_eq!(lister.children(&dir).expect("list").names, ["only"]);
        assert_ne!(lister.shell.as_ref().expect("replaced").pid(), pid);

        let _ = std::fs::remove_dir_all(&root);
    }
}
