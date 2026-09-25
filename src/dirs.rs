//! Listing directories on the host that will run the session.
//!
//! The completion half of the local/remote seam.
//! [`crate::transport::Transport`] answers questions about sessions; this
//! answers the one question the create prompt asks about the host itself —
//! what is inside this directory — and it is separate for one reason: it has
//! to leave the transport behind.
//!
//! [`crate::transport::Transport`] is a `Box<dyn Transport>` held by the picker
//! and is not `Send`. Completion runs on a worker thread (see
//! [`crate::ui::complete`], which explains why), and a thread cannot borrow the
//! picker's transport. So each transport hands out a [`DirSource`] instead: a
//! small, owned, `Send` handle carrying just enough to reach the same host
//! again. Locally that is nothing at all; over ssh it is the host and the
//! `ControlPath` of the master connection the transport already brought up, so
//! the worker's shell is a new channel on an existing connection rather than a
//! new connection.
//!
//! The worker turns it into a [`Lister`]: the source plus a shell of its own on
//! the host, kept across questions. Its own, rather than the transport's,
//! because the transport's shell is busy with what the user asked for and this
//! one answers what nvmux asked on its own account — so over ssh it is started
//! unattended, and can never sit on a prompt nobody can see.
//!
//! # One script, both hosts
//!
//! Neither arm reimplements directory listing: both run `scripts/dirs.sh`, in
//! the same kind of shell [`crate::transport::local`] runs every other script
//! in. So the dotfile rule, the sort order, the cap and the handling of odd
//! names are one implementation and cannot drift between local and remote —
//! which they would, silently, if the local side used `read_dir`.
//!
//! What makes that affordable is that a listing is a run in a shell already
//! there, not a process per question: the script itself is what a keystroke
//! costs the host, and it is not on the keypress path at all, because a worker
//! thread is what calls this.

#[cfg(unix)]
use std::path::PathBuf;

use crate::error::{NvmuxError, Result};
use crate::proc::Shell;
use crate::shell;
#[cfg(unix)]
use crate::ssh::Ssh;
use crate::transport::protocol::{self, Listing};

/// A `Send` handle that can list directories on one session host.
///
/// Cheap to make and cheap to hold: the ssh arm is a hostname and a path, not a
/// connection, and the relay's is a handle on the connection its transport
/// already has. Cloneable so a prompt can keep one and give the worker another.
#[derive(Debug, Clone)]
pub enum DirSource {
    #[cfg(unix)]
    Local,
    #[cfg(unix)]
    Ssh { host: String, control_path: PathBuf },
    /// A shell opened on the relay's one connection (see [`crate::mux`]) —
    /// on the link as it is, never a new one, so there is nothing to prompt
    /// for and a link that is down is simply no completion.
    Relay(std::sync::Arc<crate::transport::relay::LinkSlot>),
}

impl DirSource {
    /// A shell on this host for questions nobody is watching. Over ssh that
    /// means unattended: it never prompts, and gives up on a host it cannot
    /// reach — see `ssh::unattended`.
    pub fn start_shell(&self) -> Result<Shell> {
        Ok(match self {
            #[cfg(unix)]
            DirSource::Local => Shell::local()?,
            #[cfg(unix)]
            DirSource::Ssh { host, control_path } => {
                Ssh::new(host.clone(), control_path.clone()).start_unattended_shell()?
            }
            DirSource::Relay(link) => link.open_shell()?,
        })
    }
}

/// Directory listings from one host, through one shell kept across questions.
///
/// Owned by the completion worker, which is the only thing that asks. The
/// shell is started by the first question, or by [`Lister::warm`] ahead of it,
/// and replaced if it dies.
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

    /// Start the shell before there is a question for it, so what it costs to
    /// bring up — over ssh, a login shell on the host — is paid while the user
    /// is still typing rather than by the first keystroke that needs an answer.
    /// A shell that will not start is not reported here; the first question
    /// tries again and is where the failure belongs.
    pub fn warm(&mut self) {
        let _ = self.shell();
    }

    fn shell(&mut self) -> Result<&mut Shell> {
        if self.shell.as_mut().is_none_or(|shell| !shell.is_alive()) {
            self.shell = Some(self.source.start_shell()?);
        }
        Ok(self.shell.as_mut().expect("just started"))
    }

    /// Every subdirectory of `dir`, dotted ones included.
    ///
    /// The whole directory rather than a filtered slice of it, because the
    /// matching is fuzzy and lives in [`crate::ui::complete`]: a subsequence
    /// match cannot be expressed as a shell pattern, since the first character
    /// typed need not be the first character of the name. That is also what lets
    /// one answer serve every keystroke inside a directory rather than only the
    /// ones that extend the same prefix.
    ///
    /// Dotted names come back and are hidden further up, where a query can ask
    /// for them. The rule is the shell's either way; only the place it is
    /// applied has moved.
    ///
    /// A directory that is not there is an empty listing, not an error: at a
    /// prompt, half a path is the normal state of the input, and every keystroke
    /// on the way to a real directory passes through one that is not there yet.
    pub fn children(&mut self, dir: &str) -> Result<Listing> {
        // The status goes unread, as it always has: a directory that is not
        // there is the script's empty answer, and anything else it could say
        // is not worth more than the empty listing the worker shows for an
        // error. A shell that died is dropped, and the next question replaces
        // it.
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

/// Split a typed path into the part that no longer counts and the part that does.
///
/// `//` means "start again from the root", so everything up to and including the
/// last one is inert: `/home/shu//etc` is `/etc`, with `/home/shu/` left behind.
/// It is what the prompt offers instead of making the user delete a path to get
/// out of it, and the field draws the inert half dim so it reads as what it is.
///
/// **The live half is what must reach the session host.** POSIX reads `a//b` as
/// `a/b`, so `cd /home/shu//etc` would land in `/home/shu/etc` — a real
/// directory, the wrong one, and silently. Resolving the convention here, before
/// anything leaves the prompt, is what makes it safe to offer.
///
/// The split falls *between* the two slashes, so the live half keeps a leading
/// one and is therefore still absolute.
pub fn anchored(input: &str) -> (&str, &str) {
    match input.rfind("//") {
        Some(at) => input.split_at(at + 1),
        None => ("", input),
    }
}

/// Split what has been typed into the directory to list and the name to match
/// inside it.
///
/// The split is at the last `/`, which is what makes the listing reusable: every
/// keystroke inside one directory asks about the same directory, so the answer
/// can be kept and filtered rather than asked for again.
///
/// A path with no `/` at all has no directory to list — the field's rule is that
/// a directory is absolute, so there is no relative one to resolve against — and
/// this returns `None` rather than inventing a root to search.
pub fn split(input: &str) -> Option<(&str, &str)> {
    let at = input.rfind('/')?;
    let (dir, rest) = input.split_at(at);
    // `/etc` splits into `/` and `etc`, not `` and `etc`: the root is a real
    // directory and `""` is not one.
    let dir = if dir.is_empty() { "/" } else { dir };
    Some((dir, &rest[1..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_slash_starts_again_from_the_root() {
        // The case the prompt exists to serve: out of home, to somewhere else.
        assert_eq!(anchored("/home/shu//etc"), ("/home/shu/", "/etc"));
        // Everything discarded is inert, not just the home part of it.
        assert_eq!(
            anchored("/home/shu/projects//etc"),
            ("/home/shu/projects/", "/etc")
        );
        // The last one wins, so changing your mind twice still works.
        assert_eq!(anchored("/a//b//c"), ("/a//b/", "/c"));
        // A bare `//` is the root itself.
        assert_eq!(anchored("//"), ("/", "/"));
        // Three in a row is the same answer: the split is at the last pair.
        assert_eq!(anchored("///etc"), ("//", "/etc"));
        // Nothing to discard, so nothing is inert and nothing draws dim.
        assert_eq!(anchored("/home/shu/projects"), ("", "/home/shu/projects"));
        assert_eq!(anchored("/"), ("", "/"));
        assert_eq!(anchored(""), ("", ""));
        // A trailing `//` is still the root, mid-typing.
        assert_eq!(anchored("/home/shu//"), ("/home/shu/", "/"));
    }

    /// The live half keeps its leading slash, so what comes out of `anchored` is
    /// still an absolute path — which is the one thing `validate_directory` will
    /// not accept a substitute for.
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
        // Nothing to list, and nothing invented to list instead.
        assert_eq!(split("home"), None);
        assert_eq!(split(""), None);
    }

    /// A name with a space in it is one name. The split is on `/` alone, so
    /// this is only worth pinning because a whitespace-splitting version would
    /// pass every other test here.
    #[test]
    fn a_directory_name_with_a_space_in_it_survives_the_split() {
        assert_eq!(split("/home/you/my pro"), Some(("/home/you", "my pro")));
    }

    fn local() -> Lister {
        Lister::new(DirSource::Local)
    }

    /// The local arm runs the same script the remote one does, so this is also
    /// the test that the script's contract holds as embedded rather than as it
    /// sits in the checkout.
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

    /// Names a shell would otherwise split on or treat as a pattern. Each one
    /// is a real directory somebody could have, and each would be a different
    /// bug in the framing between the host and here.
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

    /// One shell answers every question — that is the point of keeping one —
    /// and a question after the shell has died gets a fresh one rather than an
    /// error for the rest of the prompt's life.
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
