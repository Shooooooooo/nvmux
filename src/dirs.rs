//! Listing directories on the host that will run the session.
//!
//! The completion half of the local/remote seam. [`Transport`] answers questions
//! about sessions; this answers the one question the create prompt asks about
//! the host itself — what is inside this directory — and it is separate for one
//! reason: it has to leave the transport behind.
//!
//! [`Transport`] is a `Box<dyn Transport>` held by the picker and is not `Send`.
//! Completion runs on a worker thread (see [`crate::ui::complete`], which
//! explains why), and a thread cannot borrow the picker's transport. So each
//! transport hands out a [`DirSource`] instead: a small, owned, `Send` handle
//! carrying just enough to ask the same host again. Locally that is nothing at
//! all; over ssh it is the host and the `ControlPath` of the master connection
//! the transport already brought up, so a listing is a new invocation on an
//! existing connection rather than a new connection.
//!
//! # One script, both hosts
//!
//! Neither arm reimplements directory listing: both run `scripts/dirs.sh`, the
//! local one through `/bin/sh` exactly as [`crate::transport::local`] runs every
//! other script. So the dotfile rule, the sort order, the cap and the handling
//! of odd names are one implementation and cannot drift between local and
//! remote — which they would, silently, if the local side used `read_dir`.
//!
//! The measured cost of the local fork is what makes that affordable: it is a
//! millisecond or so against `read_dir`'s tenth of one, and neither is on the
//! keypress path at all, because a worker thread is what calls this.

use std::path::PathBuf;

use crate::error::Result;
use crate::ssh::Ssh;
use crate::transport::protocol::{self, Listing};
use crate::{proc, shell};

/// A `Send` handle that can list directories on one session host.
///
/// Cheap to make and cheap to hold: the ssh arm is a hostname and a path, not a
/// connection. Cloneable so a prompt can keep one and give the worker another.
#[derive(Debug, Clone)]
pub enum DirSource {
    Local,
    Ssh { host: String, control_path: PathBuf },
}

impl DirSource {
    /// The subdirectories of `dir` whose names start with `prefix`.
    ///
    /// `prefix` is matched by the host's shell, not here, so a directory with a
    /// hundred thousand entries costs one small answer rather than a hundred
    /// thousand lines. An empty prefix does not match dotfiles and a prefix of
    /// `.` does, which is what a shell does and what someone typing a path
    /// expects.
    ///
    /// A directory that is not there is an empty listing, not an error: at a
    /// prompt, half a path is the normal state of the input, and every keystroke
    /// on the way to a real directory passes through one that is not there yet.
    pub fn children(&self, dir: &str, prefix: &str) -> Result<Listing> {
        let out = match self {
            DirSource::Local => proc::run_local(shell::DIRS_SCRIPT, &[dir, prefix])?,
            DirSource::Ssh { host, control_path } => {
                Ssh::new(host.clone(), control_path.clone())
                    .run_script(shell::DIRS_SCRIPT, &[dir, prefix])?
            }
        };
        protocol::parse_dirs(&out.stdout)
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

    /// The local arm runs the same script the remote one does, so this is also
    /// the test that the script's contract holds as embedded rather than as it
    /// sits in the checkout.
    #[test]
    fn the_local_source_lists_real_subdirectories() {
        let root =
            std::env::temp_dir().join(format!("nvmux-dirs-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["alpha", "beta", "beta-two", ".hidden"] {
            std::fs::create_dir_all(root.join(name)).expect("make a directory");
        }
        std::fs::write(root.join("a-file"), b"not a directory").expect("write a file");
        let dir = root.to_string_lossy().into_owned();

        let all = DirSource::Local.children(&dir, "").expect("list");
        assert_eq!(
            all.names,
            ["alpha", "beta", "beta-two"],
            "no dotfiles, no files"
        );
        assert!(!all.truncated);

        let some = DirSource::Local.children(&dir, "beta").expect("list");
        assert_eq!(
            some.names,
            ["beta", "beta-two"],
            "the host does the filtering"
        );

        let dotted = DirSource::Local.children(&dir, ".").expect("list");
        assert_eq!(dotted.names, [".hidden"], "a leading dot asks for dotfiles");

        // Half-typed paths are the normal state of a prompt, not an error.
        let missing = DirSource::Local
            .children(&root.join("nope").to_string_lossy(), "")
            .expect("a directory that is not there is an empty answer");
        assert!(missing.names.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Names a shell would otherwise treat as patterns, or split on. Each one
    /// is a real directory somebody could have, and each would be a different
    /// bug: a glob metacharacter in the prefix, a space in the name.
    #[test]
    fn odd_directory_names_are_listed_literally() {
        let root =
            std::env::temp_dir().join(format!("nvmux-dirs-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["a b", "c*d", "c?d", "cxd"] {
            std::fs::create_dir_all(root.join(name)).expect("make a directory");
        }
        let dir = root.to_string_lossy().into_owned();

        let spaced = DirSource::Local.children(&dir, "a").expect("list");
        assert_eq!(spaced.names, ["a b"], "a name with a space is one name");

        let starred = DirSource::Local.children(&dir, "c*").expect("list");
        assert_eq!(
            starred.names,
            ["c*d"],
            "a `*` in the prefix is a character, not a pattern"
        );

        let queried = DirSource::Local.children(&dir, "c?").expect("list");
        assert_eq!(queried.names, ["c?d"], "and neither is a `?`");

        let _ = std::fs::remove_dir_all(&root);
    }
}
