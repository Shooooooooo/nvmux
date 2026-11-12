//! Shell quoting, and the scripts that run on the session host.
//!
//! A remote spawn crosses two shells: `ssh host cmd` joins its arguments with
//! spaces and the login shell parses them again, so bad quoting is a command
//! injection. No command string is built with interpolated values: scripts
//! are fixed text on stdin and values arrive as positional arguments. The one
//! place [`quote`] embeds values is the `set --` list [`crate::proc::Shell`]
//! writes ahead of each script, read once by a known POSIX `sh`.

/// Single-quote a value so a POSIX shell reads it as exactly one literal word.
/// Single quotes have no escapes, so a `'` becomes `'\''`.
pub fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'-' | b'.' | b'/' | b',' | b':' | b'=' | b'@' | b'+'
                )
        })
    {
        // Safe unquoted; keeps logs readable.
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

pub fn quote_all<I, S>(args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .map(|a| quote(a.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Wrap a command so it runs under the user's login shell (for
/// `ssh::shell_args`).
///
/// sshd runs `<login shell> -c <command>`, and the login shell may not be
/// POSIX: fish refuses `${`, csh and tcsh refuse `:-`. So it gets only
/// `exec sh -c` and one single-quoted word without `\`, `'` or `!`, which sh,
/// bash, zsh, ksh, fish, csh and tcsh all read as the same literal:
///
/// ```text
/// exec sh -c 'case $SHELL in *csh) exec "$SHELL" -c "<inner>";; *) exec "${SHELL:-/bin/bash}" -l -c "<inner>";; esac'
/// ```
///
/// `$SHELL` is quoted in case it holds a space. The csh family gets no `-l`:
/// tcsh refuses it beside other flags and csh still skips `.login`; both read
/// `.cshrc` under `-c`.
///
/// Panics unless `inner` is plain words needing no quoting, since it sits in
/// double quotes inside that word. Values go on stdin instead.
pub fn login_shell_wrapper(inner: &str) -> String {
    assert!(
        inner.split(' ').all(|w| quote(w) == w),
        "{inner:?} is parsed by two different shells and must need no quoting"
    );
    format!(
        r#"exec sh -c 'case $SHELL in *csh) exec "$SHELL" -c "{inner}";; *) exec "${{SHELL:-/bin/bash}}" -l -c "{inner}";; esac'"#
    )
}

/// Prepends `scripts/_prelude.sh` so each script reaches the host as one
/// self-contained program. Run from a checkout, a script sources it instead.
macro_rules! with_prelude {
    ($($tail:expr),+ $(,)?) => {
        concat!(include_str!("../scripts/_prelude.sh"), $($tail),+)
    };
}

macro_rules! script {
    ($file:literal) => {
        with_prelude!(include_str!($file))
    };
}

/// Lists every session on the host, liveness included, in one invocation:
/// each `ssh` round trip costs ~230 ms even to localhost.
pub const LIST_SCRIPT: &str = script!("../scripts/list.sh");

pub const SPAWN_SCRIPT: &str = script!("../scripts/spawn.sh");

pub const KILL_SCRIPT: &str = script!("../scripts/kill.sh");

/// Probes a remote host and lists its sessions in one round trip. `hello.sh`
/// leaves the runtime directory in `$1` for `list.sh`, so the first listing
/// runs the same code as every refresh.
pub const HELLO_SCRIPT: &str = with_prelude!(
    include_str!("../scripts/hello.sh"),
    include_str!("../scripts/list.sh")
);

/// Writes `<id>.json` on the session host, for the SSH transport.
pub const WRITE_META_SCRIPT: &str = script!("../scripts/write_meta.sh");

/// Rewrites the numbers of reordered sessions in one run. Separate from
/// `write_meta.sh` because it may only edit existing metadata, never create
/// it (see its note on orphans).
pub const RENUMBER_SCRIPT: &str = script!("../scripts/renumber.sh");

/// Lists the subdirectories of one directory for the create prompt's
/// completion: one round trip per `/` typed, not per keystroke.
pub const DIRS_SCRIPT: &str = script!("../scripts/dirs.sh");

#[cfg(test)]
const SCRIPTS: &[(&str, &str)] = &[
    ("list.sh", LIST_SCRIPT),
    ("dirs.sh", DIRS_SCRIPT),
    ("spawn.sh", SPAWN_SCRIPT),
    ("kill.sh", KILL_SCRIPT),
    ("hello.sh", HELLO_SCRIPT),
    ("write_meta.sh", WRITE_META_SCRIPT),
    ("renumber.sh", RENUMBER_SCRIPT),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::Output;
    use crate::test_support::{scratch_dir, scratch_path, scratch_sock};
    use std::process::{Child, Command};

    /// Round-trip a word through a real `/bin/sh`.
    fn sh_echo(word: &str) -> String {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {word}"))
            .output()
            .expect("run /bin/sh");
        assert!(
            out.status.success(),
            "sh rejected {word:?}: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8")
    }

    #[test]
    fn simple_words_are_left_bare() {
        assert_eq!(quote("dotfiles"), "dotfiles");
        assert_eq!(quote("api-server"), "api-server");
        assert_eq!(
            quote("/tmp/nvmux-501/abcdefgh.sock"),
            "/tmp/nvmux-501/abcdefgh.sock"
        );
        assert_eq!(quote("user@host"), "user@host");
    }

    #[test]
    fn a_real_shell_recovers_the_original_string() {
        for original in [
            "simple",
            "my project",
            "it's",
            r#"say "hi""#,
            r#"both ' and " quotes"#,
            "$HOME",
            "$(rm -rf /)",
            "`whoami`",
            "a;b",
            "a|b",
            "a&b",
            "a>b",
            "new\nline",
            "tab\there",
            "back\\slash",
            "*",
            "~",
            "!history",
            "日本語",
            "emoji 🎉",
            "--not-a-flag",
            "",
        ] {
            assert_eq!(
                sh_echo(&quote(original)),
                original,
                "round trip failed for {original:?} -> {}",
                quote(original)
            );
        }
    }

    #[test]
    fn injection_attempts_stay_inert() {
        // If quoting were wrong the subshell would run.
        let evil = "x$(touch /tmp/nvmux-pwned-$$)y";
        assert_eq!(sh_echo(&quote(evil)), evil);

        let evil2 = "'; touch /tmp/nvmux-pwned2; echo '";
        assert_eq!(sh_echo(&quote(evil2)), evil2);
    }

    #[test]
    fn quote_all_produces_separable_words() {
        let joined = quote_all(["one", "two words", "it's three"]);
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(r#"for a in {joined}; do printf '[%s]' "$a"; done"#))
            .output()
            .expect("run sh");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "[one][two words][it's three]"
        );
    }

    /// The login shell may be fish or tcsh: it must see plain words and one
    /// single-quoted word holding every `$`, without the `\` (fish) or `!`
    /// (csh) they act on inside single quotes. `ssh`'s tests run the shells.
    #[test]
    fn the_login_shell_sees_plain_words_and_one_literal() {
        let w = login_shell_wrapper("exec sh -s");
        let parts: Vec<&str> = w.split('\'').collect();
        assert_eq!(parts.len(), 3, "exactly one single-quoted word: {w}");
        let (outside, literal) = (format!("{} {}", parts[0], parts[2]), parts[1]);
        assert!(!outside.contains("${"), "the login shell sees ${{: {w}");
        assert!(
            outside.split_whitespace().all(|word| quote(word) == word),
            "the login shell sees more than plain words: {outside:?}"
        );
        assert!(
            !literal.contains(['\\', '!', '\n']),
            "not a literal to fish and csh: {literal:?}"
        );
    }

    #[test]
    fn login_wrapper_uses_the_users_shell_with_bash_fallback() {
        let w = login_shell_wrapper("true");
        assert!(w.contains(r#""${SHELL:-/bin/bash}""#), "no fallback in {w}");
        assert!(w.contains(r#" -l -c "true""#), "not a login shell in {w}");
        assert!(
            w.starts_with("exec sh -c '"),
            "the sh layer is missing: {w}"
        );
    }

    #[test]
    #[should_panic(expected = "must need no quoting")]
    fn an_inner_command_that_needs_quoting_is_refused() {
        login_shell_wrapper(r#"printf %s "$HOME""#);
    }

    /// Strip comment lines and POSIX character classes for the bashism scan:
    /// `[[:space:]]` contains `[[` and `]]`, and comments say "local".
    fn code_only(script: &str) -> String {
        script
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
            .replace("[[:alpha:]]", "")
            .replace("[[:digit:]]", "")
            .replace("[[:space:]]", "")
            .replace("[[:alnum:]]", "")
    }

    /// Without the prelude a script dies on its first `finish`: "not found".
    #[test]
    fn every_script_ships_with_exactly_one_prelude() {
        for &(name, body) in SCRIPTS {
            assert_eq!(
                body.matches("NVMUX_PRELUDE=1").count(),
                1,
                "{name} should carry the prelude exactly once"
            );
            assert!(
                body.contains("finish()"),
                "{name} is missing the prelude's helpers"
            );
        }
    }

    /// Run a script as `ssh` delivers it: on stdin, with `args` as `$1..$n`.
    fn run_script(body: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-s").args(args);
        crate::proc::run_feeding_stdin(&mut cmd, body).expect("run the script")
    }

    /// Run as `sh -c <STANDIN> nvim --headless --listen <sock>`: the trailing
    /// words are unused `$0..$n`, so the command line looks like a session's,
    /// which is all the scripts check. No Neovim needed.
    ///
    /// The trailing `:` stops bash (`/bin/sh` on macOS; dash does not do this)
    /// from exec'ing a lone simple command in place of itself, which would
    /// replace that argv with `sleep 30`.
    const STANDIN: &str = "sleep 30; :";

    /// A pid's command line, read with the prelude's own `cmdline`.
    fn cmdline_of(pid: u32) -> String {
        let script = with_prelude!("cmdline \"$1\"\n");
        crate::proc::run_local(script, &[&pid.to_string()])
            .expect("read the command line")
            .stdout
    }

    /// A process whose command line reads like a session serving `sock`, with
    /// `trailing` after it. Panics if the shell dropped that argv, which would
    /// otherwise surface as several unrelated test failures.
    fn spawn_standin(sock: &str, trailing: &[&str]) -> Child {
        spawn_standin_as(None, sock, trailing)
    }

    /// [`spawn_standin`], run as `uid` where given, which needs root.
    fn spawn_standin_as(uid: Option<u32>, sock: &str, trailing: &[&str]) -> Child {
        use std::os::unix::process::CommandExt;

        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", STANDIN, "nvim", "--headless", "--listen", sock])
            .args(trailing);
        if let Some(uid) = uid {
            // Out of the test's directory, which `uid` may not enter.
            cmd.uid(uid).gid(uid).current_dir("/");
        }
        let mut child = cmd.spawn().expect("spawn a stand-in session");
        let seen = cmdline_of(child.id());
        if !seen.contains(&format!("--listen {sock}")) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the stand-in must carry the socket on its command line: {seen:?}");
        }
        child
    }

    /// `HELLO_SCRIPT` is three files concatenated and must read as a probe,
    /// then a listing, under one terminator. A `finish` in `hello.sh` would
    /// add a second that claims the listing done before `list.sh` started.
    #[test]
    fn the_greeting_carries_a_listing_under_one_terminator() {
        use crate::transport::protocol;

        // No argument: `hello.sh` finds the runtime directory itself.
        let out = run_script(HELLO_SCRIPT, &[]);
        assert!(
            out.stdout.matches("NVMUX_END").count() == 1,
            "one terminator, and it is list.sh's: {:?}",
            out.stdout
        );

        let probe = protocol::parse_probe(&out.stdout).expect("reads as a probe");
        assert_eq!(
            std::path::Path::new(&probe.runtime_dir),
            crate::paths::runtime_dir(),
            "the greeting must name the directory `paths` would"
        );
        protocol::parse_listing(&out.stdout).expect("reads as a listing");
    }

    /// Must never create a file: an orphan (a live socket without metadata)
    /// shows under a synthesised name, and writing it here would make that
    /// placeholder real.
    #[test]
    fn renumber_writes_every_pair_and_never_creates_metadata() {
        use crate::session::Session;

        let dir = scratch_dir("shell-renumber");

        let meta = |id: &str, num: u32| {
            let mut s = Session::new(id.to_string(), format!("name-{id}"), 0, num);
            s.created = 100;
            s
        };
        for (id, num) in [("aaaaaaaa", 1), ("bbbbbbbb", 2)] {
            meta(id, num)
                .write_atomic(&dir.join(format!("{id}.json")))
                .expect("seed metadata");
        }

        // The third pair names a session with no metadata at all.
        let swapped = [
            meta("aaaaaaaa", 2).to_json().expect("json"),
            meta("bbbbbbbb", 1).to_json().expect("json"),
            meta("cccccccc", 3).to_json().expect("json"),
        ];
        let out = run_script(
            RENUMBER_SCRIPT,
            &[
                &dir.to_string_lossy(),
                "aaaaaaaa",
                &swapped[0],
                "bbbbbbbb",
                &swapped[1],
                "cccccccc",
                &swapped[2],
            ],
        );
        crate::transport::protocol::parse_write(&out.stdout, "renumber").expect("the script ran");

        let num_of = |id: &str| -> u32 {
            let path = dir.join(format!("{id}.json"));
            let bytes = std::fs::read(&path).expect("read back");
            Session::from_json(&bytes, &path).expect("parse").num
        };
        assert_eq!(num_of("aaaaaaaa"), 2, "the first pair was not written");
        assert_eq!(num_of("bbbbbbbb"), 1, "the second pair was not written");
        assert!(
            !dir.join("cccccccc.json").exists(),
            "metadata was conjured for a session that had none"
        );

        let strays: Vec<_> = std::fs::read_dir(&dir)
            .expect("readdir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "left temp files behind: {strays:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In world-writable /tmp another user can create the runtime directory
    /// first, and over ssh nothing but the scripts checks it. An owner other
    /// than us needs a second uid to test; a mode does not.
    #[test]
    fn every_script_refuses_a_runtime_directory_others_can_reach() {
        use crate::transport::protocol;
        use std::os::unix::fs::PermissionsExt;

        let record = r#"{"id":"aaaaaaaa","name":"a","created":1,"pid":1,"num":1}"#;
        let changed = r#"{"id":"aaaaaaaa","name":"b","created":1,"pid":1,"num":2}"#;
        for mode in [0o755, 0o770, 0o703] {
            let dir = scratch_dir(&format!("shell-open-{mode:o}"));
            std::fs::write(dir.join("aaaaaaaa.sock"), "").expect("socket stand-in");
            std::fs::write(dir.join("aaaaaaaa.json"), record).expect("seed metadata");
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let arg = dir.to_string_lossy().into_owned();

            let outcomes = [
                (
                    "list.sh",
                    protocol::parse_listing(&run_script(LIST_SCRIPT, &[&arg]).stdout).map(drop),
                ),
                (
                    "write_meta.sh",
                    protocol::parse_write(
                        &run_script(WRITE_META_SCRIPT, &[&arg, "aaaaaaaa", changed]).stdout,
                        "metadata write",
                    ),
                ),
                (
                    "renumber.sh",
                    protocol::parse_write(
                        &run_script(RENUMBER_SCRIPT, &[&arg, "aaaaaaaa", changed]).stdout,
                        "renumber",
                    ),
                ),
                (
                    "kill.sh",
                    protocol::parse_kill(&run_script(KILL_SCRIPT, &[&arg, "aaaaaaaa", ""]).stdout)
                        .map(drop),
                ),
                (
                    "spawn.sh",
                    protocol::parse_spawn(
                        &run_script(SPAWN_SCRIPT, &[&arg, "bbbbbbbb", "/", "/bin/sh", "-c", ":"])
                            .stdout,
                    )
                    .map(drop),
                ),
            ];
            let kept = std::fs::read_to_string(dir.join("aaaaaaaa.json"));
            let _ = std::fs::remove_dir_all(&dir);

            for (name, outcome) in outcomes {
                let Err(err) = outcome else {
                    panic!("{name} used a runtime directory with mode {mode:o}");
                };
                assert!(
                    err.to_string().contains("accessible to other users"),
                    "{name} under {mode:o}: {err}"
                );
            }
            assert_eq!(
                kept.ok().as_deref(),
                Some(record),
                "the metadata was rewritten or removed under {mode:o}"
            );
        }
    }

    const CHECK_DIR: &str =
        with_prelude!("if require_private_dir \"$1\"; then echo private; else echo missing; fi\n");

    /// Anyone can create the directory the moment after the check, so a
    /// caller that took "missing" for "checked" would go on to use a
    /// directory nobody checked.
    #[test]
    fn a_missing_runtime_directory_is_not_reported_as_private() {
        let missing = scratch_path("shell-dir-missing");
        let private = scratch_dir("shell-dir-private");
        let gone = run_script(CHECK_DIR, &[&missing.to_string_lossy()]).stdout;
        let ours = run_script(CHECK_DIR, &[&private.to_string_lossy()]).stdout;
        let _ = std::fs::remove_dir_all(&private);

        assert_eq!(gone, "missing\n");
        assert_eq!(ours, "private\n");
    }

    /// As in `paths::ensure_dir_secure`. A directory made in a setgid parent
    /// inherits the bit, and neither bit lets anyone else in.
    #[test]
    fn a_setgid_or_sticky_runtime_directory_is_still_private() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        for mode in [0o2700, 0o1700] {
            let dir = scratch_dir(&format!("shell-dir-{mode:o}"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).expect("chmod");
            let set = std::fs::metadata(&dir).expect("stat").mode() & 0o7777;
            let out = run_script(CHECK_DIR, &[&dir.to_string_lossy()]).stdout;
            let _ = std::fs::remove_dir_all(&dir);

            assert_eq!(set, mode, "chmod {mode:o} did not stick");
            assert_eq!(out, "private\n", "under {mode:o}");
        }
    }

    /// The cause, such as a full disk or a file in the way, is what the user
    /// can act on.
    #[test]
    fn a_runtime_directory_that_cannot_be_made_is_refused_with_the_cause() {
        use crate::transport::protocol;

        let parent = scratch_path("shell-not-a-dir");
        std::fs::write(&parent, "").expect("a file where a directory goes");
        let arg = parent.join("nvmux").to_string_lossy().into_owned();
        let outcomes = [
            (
                "spawn.sh",
                protocol::parse_spawn(
                    &run_script(SPAWN_SCRIPT, &[&arg, "bbbbbbbb", "/", "/bin/sh", "-c", ":"])
                        .stdout,
                )
                .map(drop),
            ),
            (
                "write_meta.sh",
                protocol::parse_write(
                    &run_script(WRITE_META_SCRIPT, &[&arg, "aaaaaaaa", "{}"]).stdout,
                    "metadata write",
                ),
            ),
        ];
        let _ = std::fs::remove_file(&parent);

        let bare = format!("could not create runtime directory {arg}: ");
        for (name, outcome) in outcomes {
            let err = outcome
                .expect_err("made a directory under a file")
                .to_string();
            assert!(
                err.strip_prefix(&bare)
                    .is_some_and(|cause| !cause.is_empty()),
                "{name}: {err}"
            );
        }
    }

    /// `$$` shows in `ps`, so the temp name can be known before the write.
    #[test]
    fn a_symlink_planted_at_the_temp_name_is_not_written_through() {
        use std::io::Write;
        use std::process::Stdio;

        let dir = scratch_dir("shell-planted");
        let target = dir.join("precious");
        let json = dir.join("aaaaaaaa.json");
        let arg = dir.to_string_lossy().into_owned();
        let record = r#"{"id":"aaaaaaaa","name":"a","created":1,"pid":1,"num":2}"#;

        for (name, script) in [
            ("write_meta.sh", WRITE_META_SCRIPT),
            ("renumber.sh", RENUMBER_SCRIPT),
        ] {
            std::fs::write(&target, "precious\n").expect("target");
            let _ = std::fs::remove_file(&json);
            std::fs::write(&json, record).expect("seed metadata");

            let mut sh = Command::new("/bin/sh")
                .arg("-s")
                .args([arg.as_str(), "aaaaaaaa", record])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start sh");
            let planted = dir.join(format!("aaaaaaaa.json.tmp{}", sh.id()));
            std::os::unix::fs::symlink(&target, &planted).expect("plant");
            sh.stdin
                .take()
                .expect("stdin")
                .write_all(script.as_bytes())
                .expect("feed the script");
            let out = sh.wait_with_output().expect("run the script");
            let _ = std::fs::remove_file(&planted);

            assert_eq!(
                std::fs::read_to_string(&target).expect("read the target"),
                "precious\n",
                "{name} wrote through the symlink: {}",
                String::from_utf8_lossy(&out.stdout)
            );
            assert!(
                !std::fs::symlink_metadata(&json)
                    .expect("stat the metadata")
                    .file_type()
                    .is_symlink(),
                "{name} renamed the symlink into place"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The greeting runs in the user's login shell, whose environment is
    /// arbitrary. From a checkout `hello.sh` execs `list.sh` by a path in a
    /// variable; read with a `:-` default, an inherited value would be exec'd
    /// and the greeting would lack its terminator.
    #[test]
    fn the_greeting_reads_nothing_from_the_environment() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-s")
            .env("standalone", "/nonexistent-injected-path")
            .env("NVMUX_STANDALONE", "/nonexistent-injected-path");
        let out = crate::proc::run_feeding_stdin(&mut cmd, HELLO_SCRIPT).expect("run");
        crate::transport::protocol::parse_probe(&out.stdout)
            .expect("a polluted environment must not stop the greeting");
    }

    #[test]
    fn a_listing_finds_every_live_session_and_none_of_the_dead() {
        let dir = scratch_dir("shell-listing");
        let live = ["aaaaaaaa", "bbbbbbbb", "cccccccc"];
        let dead = "dddddddd";

        let mut held = Vec::new();
        for id in live {
            let sock = dir.join(format!("{id}.sock"));
            std::fs::write(&sock, "").expect("socket stand-in");
            held.push(spawn_standin(&sock.to_string_lossy(), &[]));
        }
        std::fs::write(dir.join(format!("{dead}.sock")), "").expect("socket stand-in");

        let out = run_script(LIST_SCRIPT, &[&dir.to_string_lossy()]);
        for child in &mut held {
            let _ = child.kill();
            let _ = child.wait();
        }
        let rows = crate::transport::protocol::parse_listing(&out.stdout).expect("a listing");
        let _ = std::fs::remove_dir_all(&dir);

        for id in live {
            let row = rows.iter().find(|r| r.id == id);
            assert!(
                row.is_some_and(|r| r.pid_alive),
                "{id} is being served and must be listed alive: {:?}",
                out.stdout
            );
        }
        assert!(
            rows.iter().any(|r| r.id == dead && !r.pid_alive),
            "{dead} is served by nothing: {:?}",
            out.stdout
        );
    }

    const SERVING_PID: &str = with_prelude!("serving_pid \"$1\"\n");

    /// A user command may put arguments after the socket, which the `serving`
    /// snapshot cannot match (`listen_args` takes the rest of the line, as a
    /// runtime directory may contain a space). `serving_pid`, the fallback for
    /// the listing and `kill.sh`, must match it anywhere, or a live session
    /// reads as stale, has its files swept, and can never be killed.
    #[test]
    fn a_session_is_found_by_its_socket_wherever_it_sits_on_the_command_line() {
        let sock = scratch_sock("shell-trailing");
        let sock = sock.to_string_lossy().into_owned();

        for trailing in [&[][..], &["--clean"][..], &["--clean", "-u", "NONE"][..]] {
            let mut held = spawn_standin(&sock, trailing);
            let found = crate::proc::run_local(SERVING_PID, &[&sock])
                .expect("ask")
                .stdout;
            let _ = held.kill();
            let _ = held.wait();
            assert_eq!(
                found.trim(),
                held.id().to_string(),
                "not found with {trailing:?} after the socket"
            );
        }
    }

    /// `serving_pid`'s answer for `sock` from `ps` and, where there is a
    /// `/proc`, from the fallback that hosts without `ps` take.
    fn serving_pid_answers(sock: &str, tag: &str) -> Vec<(&'static str, String)> {
        answers(SERVING_PID, &[sock], tag)
    }

    /// What `script` prints for `args` with `ps` and, where there is a
    /// `/proc`, without it.
    fn answers(script: &str, args: &[&str], tag: &str) -> Vec<(&'static str, String)> {
        let mut answers = vec![("ps", run_script(script, args).stdout)];
        if std::path::Path::new("/proc/self/cmdline").exists() {
            // A host without `ps` but with the usual tools.
            let bin = scratch_dir(tag);
            let path = std::env::var_os("PATH").unwrap_or_default();
            for tool in [
                "awk", "cat", "cut", "grep", "head", "id", "ls", "sed", "sort", "tr",
            ] {
                let found = std::env::split_paths(&path)
                    .map(|dir| dir.join(tool))
                    .find(|found| found.is_file());
                if let Some(found) = found {
                    std::os::unix::fs::symlink(found, bin.join(tool)).expect("link a tool");
                }
            }
            let mut sh = Command::new("/bin/sh");
            sh.arg("-s").args(args).env("PATH", &bin);
            let out = crate::proc::run_feeding_stdin(&mut sh, script).expect("ask");
            let _ = std::fs::remove_dir_all(&bin);
            answers.push(("/proc", out.stdout));
        }
        answers
    }

    /// Start a stand-in serving `sock`, with `trailing` after the socket, and
    /// panic unless every lookup finds it.
    fn assert_found(sock: &str, trailing: &[&str], tag: &str) {
        let mut held = spawn_standin(sock, trailing);
        let answers = serving_pid_answers(sock, tag);
        let _ = held.kill();
        let _ = held.wait();
        for (how, found) in answers {
            assert_eq!(found.trim(), held.id().to_string(), "not found by {how}");
        }
    }

    /// A lookup that drops lines saying "grep", to skip its own `grep`, drops
    /// this session too: the listing sweeps its files and `kill.sh` calls it
    /// absent, leaving it running.
    #[test]
    fn a_session_whose_command_line_says_grep_is_found() {
        let sock = scratch_sock("shell-says");
        assert_found(
            &sock.to_string_lossy(),
            &["--cmd", "set grepprg=rg"],
            "shell-says-bin",
        );
    }

    /// `awk -v` would read its `\t` as a tab.
    #[test]
    fn a_socket_path_with_a_backslash_is_matched_as_written() {
        let sock = scratch_sock(r"shell-back\tslash");
        assert_found(&sock.to_string_lossy(), &[], "shell-backslash-bin");
    }

    /// Asked for `a.sock`, `kill.sh` would end the session at `a.sock2`.
    #[test]
    fn a_longer_path_is_another_socket() {
        let sock = scratch_sock("shell-longer");
        let sock = sock.to_string_lossy().into_owned();
        let longer = format!("{sock}2");
        let mut held = spawn_standin(&longer, &[]);
        let asked = serving_pid_answers(&sock, "shell-longer-bin");
        let own = serving_pid_answers(&longer, "shell-longer-bin");
        let _ = held.kill();
        let _ = held.wait();
        for (how, found) in asked {
            assert_eq!(found, "", "{how} matched {longer} when asked for {sock}");
        }
        for (how, found) in own {
            assert_eq!(
                found.trim(),
                held.id().to_string(),
                "{longer} not found by {how}"
            );
        }
    }

    /// A grep for the socket names it too, and started first it has the lower
    /// pid: taken for the session, it is what `kill.sh` would end, leaving
    /// nvim running with its files gone.
    #[test]
    fn a_grep_for_the_socket_is_not_the_session() {
        use std::process::Stdio;

        let sock = scratch_sock("shell-sought");
        let sock = sock.to_string_lossy().into_owned();
        // Blocks reading its stdin, which closes if this test panics.
        let mut grep = Command::new("grep")
            .args(["-q", "-F", "--", &format!("--listen {sock}")])
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn grep");
        let alone = serving_pid_answers(&sock, "shell-sought-bin");
        assert_found(&sock, &[], "shell-sought-bin");
        let _ = grep.kill();
        let _ = grep.wait();
        for (how, found) in alone {
            assert_eq!(found, "", "{how} took the grep for a session");
        }
    }

    /// macOS's `ps` prints such a byte raw, and its awk exits when a regex
    /// meets one: a lookup that tested every line's argv[0] found nothing past
    /// this process. Linux's `ps` prints a `?`, so only macOS can fail this.
    #[test]
    fn a_session_is_found_past_a_command_line_the_locale_cannot_decode() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::process::CommandExt;

        // Started first, so `ps` lists it before the session.
        let mut decoy = Command::new("sleep")
            .arg0(OsStr::from_bytes(b"caf\xe9"))
            .arg("30")
            .spawn()
            .expect("spawn the decoy");
        let sock = scratch_sock("shell-undecodable");
        let sock = sock.to_string_lossy().into_owned();
        let mut held = spawn_standin(&sock, &[]);
        let utf8 = if cfg!(target_os = "macos") {
            "en_US.UTF-8"
        } else {
            "C.UTF-8"
        };
        let mut sh = Command::new("/bin/sh");
        sh.arg("-s").arg(&sock).env("LC_ALL", utf8);
        let out = crate::proc::run_feeding_stdin(&mut sh, SERVING_PID).expect("ask");
        for child in [&mut held, &mut decoy] {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert_eq!(out.stdout.trim(), held.id().to_string(), "{}", out.stderr);
    }

    const OWNS_SOCKET: &str =
        with_prelude!("if owns_socket \"$1\" \"$2\"; then echo yes; else echo no; fi\n");

    /// A recorded pid is trusted only if it owns the socket: reused by the
    /// session at `a.sock2`, it must not pass for `a.sock`'s, or `kill.sh`
    /// ends the wrong session.
    #[test]
    fn a_session_owns_its_socket_and_not_a_prefix_of_it() {
        let sock = scratch_sock("shell-owns");
        let sock = sock.to_string_lossy().into_owned();
        let longer = format!("{sock}2");
        let mut held = spawn_standin(&longer, &["--cmd", "set grepprg=rg"]);
        let pid = held.id().to_string();
        let own = run_script(OWNS_SOCKET, &[&pid, &longer]).stdout;
        let prefix = run_script(OWNS_SOCKET, &[&pid, &sock]).stdout;
        let _ = held.kill();
        let _ = held.wait();
        assert_eq!(own, "yes\n", "{pid} serves {longer}");
        assert_eq!(prefix, "no\n", "{pid} serves {longer}, not {sock}");
    }

    /// Its own processes are in the table it reads, and a matcher carrying the
    /// flag and path, as `grep -F -- "--listen $1"` does, finds itself.
    #[test]
    fn the_lookup_does_not_find_its_own_processes() {
        let sock = scratch_sock("shell-unserved");
        let sock = sock.to_string_lossy().into_owned();
        for (how, found) in serving_pid_answers(&sock, "shell-unserved-bin") {
            assert_eq!(found, "", "{how}: nothing serves {sock}");
        }
    }

    const SERVING: &str = with_prelude!("if serving \"$1\"; then echo yes; else echo no; fi\n");

    /// Another user's process can name our socket. Taken for the session, it
    /// would keep a dead one alive and be what `kill.sh` signals. With `id`
    /// answering another uid, a stand-in of ours plays it without root.
    #[test]
    fn a_process_of_another_uid_is_not_a_session() {
        use crate::transport::protocol::{parse_kill, KillOutcome};

        // Missing, as `kill.sh` refuses a runtime directory `id` disowns.
        let dir = scratch_path("shell-theirs");
        let arg = dir.to_string_lossy().into_owned();
        let sock = format!("{arg}/aaaaaaaa.sock");
        let mut held = spawn_standin(&sock, &[]);
        let pid = held.id().to_string();
        let other = nix::unistd::geteuid().as_raw().wrapping_add(1);
        let as_other = |script: &str| format!("id() {{ echo {other}; }}\n{script}");

        let found = serving_pid_answers(&sock, "shell-theirs-bin");
        let snapped = answers(SERVING, &[&sock], "shell-theirs-bin");
        let looked = answers(&as_other(SERVING_PID), &[&sock], "shell-theirs-bin");
        let listed = answers(&as_other(SERVING), &[&sock], "shell-theirs-bin");
        let owned = answers(&as_other(OWNS_SOCKET), &[&pid, &sock], "shell-theirs-bin");
        // Last, as taking the stand-in ends it.
        let out = run_script(&as_other(KILL_SCRIPT), &[&arg, "aaaaaaaa", ""]);
        let spared = matches!(held.try_wait(), Ok(None));
        let _ = held.kill();
        let _ = held.wait();

        for (how, found) in found {
            assert_eq!(found.trim(), pid, "{how} must find it under our own uid");
        }
        // Not a socket, so only the snapshot can say yes.
        for (how, said) in snapped {
            assert_eq!(
                said, "yes\n",
                "{how}: the snapshot missed it under our own uid"
            );
        }
        for (how, found) in looked {
            assert_eq!(
                found, "",
                "{how} took another uid's process for the session"
            );
        }
        for (how, said) in listed {
            assert_eq!(said, "no\n", "{how}: another uid's process kept it alive");
        }
        for (how, said) in owned {
            assert_eq!(
                said, "no\n",
                "{how}: another uid's process owned the socket"
            );
        }
        let outcome = parse_kill(&out.stdout);
        assert!(
            matches!(outcome, Ok(KillOutcome::Absent)) && spared,
            "kill.sh signalled another uid's process: {outcome:?}"
        );
    }

    /// `/proc` keeps a newline in an argument, and a line after it that starts
    /// with our uid would pass for a command line of ours.
    #[test]
    fn an_argument_cannot_forge_a_command_line_of_ours() {
        let sock = scratch_sock("shell-forged");
        let sock = sock.to_string_lossy().into_owned();
        let forged = format!("{sock}x");
        let other = nix::unistd::geteuid().as_raw().wrapping_add(1);
        let mut held = spawn_standin(&sock, &[&format!("-c\n{other} --listen {forged}")]);
        let script = format!("id() {{ echo {other}; }}\n{SERVING}");
        let said = answers(&script, &[&forged], "shell-forged-bin");
        let _ = held.kill();
        let _ = held.wait();

        for (how, said) in said {
            assert_eq!(said, "no\n", "{how} read a forged line as uid {other}'s");
        }
    }

    /// `ps` as FreeBSD and NetBSD read `-o`: all of an argument after its
    /// first `=` is that column's header, which this drops.
    const BSD_PS: &str = r#"ps() {
  for bsd_arg; do
    shift
    case $bsd_arg in *=*) bsd_arg="${bsd_arg%%=*}=" ;; esac
    set -- "$@" "$bsd_arg"
  done
  command ps "$@"
}
"#;

    #[test]
    fn a_session_is_found_by_the_freebsd_and_netbsd_ps() {
        let sock = scratch_sock("shell-bsd");
        let sock = sock.to_string_lossy().into_owned();
        let mut held = spawn_standin(&sock, &[]);
        let found = run_script(&format!("{BSD_PS}{SERVING_PID}"), &[&sock]).stdout;
        let listed = run_script(&format!("{BSD_PS}{SERVING}"), &[&sock]).stdout;
        let _ = held.kill();
        let _ = held.wait();

        assert_eq!(found.trim(), held.id().to_string(), "serving_pid missed it");
        // Not a socket, so only the snapshot can say yes.
        assert_eq!(listed, "yes\n", "the snapshot missed it");
    }

    /// Uid 3000000000 as `id -u` prints it, and as procps and macOS's `ps`
    /// print it (`%d`) on our own processes.
    const BIG_UID: &str = r#"id() { echo 3000000000; }
ps() {
  command ps "$@" | awk -v me="$(command id -u)" '$1 == me { sub(/[0-9]+/, "-1294967296") } { print }'
}
"#;

    /// Compared as printed, our processes past uid 2^31-1 read as another
    /// uid's: `kill.sh` with no recorded pid called a live session absent, and
    /// a listing swept its files where `/proc/net/unix` could not vouch for it.
    #[test]
    fn a_session_is_found_under_a_uid_ps_prints_negative() {
        let sock = scratch_sock("shell-big-uid");
        let sock = sock.to_string_lossy().into_owned();
        let mut held = spawn_standin(&sock, &[]);
        let found = run_script(&format!("{BIG_UID}{SERVING_PID}"), &[&sock]).stdout;
        let listed = run_script(&format!("{BIG_UID}{SERVING}"), &[&sock]).stdout;
        let _ = held.kill();
        let _ = held.wait();

        assert_eq!(found.trim(), held.id().to_string(), "serving_pid missed it");
        // Not a socket, so only the snapshot can say yes.
        assert_eq!(listed, "yes\n", "the snapshot missed it");
    }

    /// `kill` refuses every signal here, as it does on another user's process.
    /// Read as an exit, that would have the session called killed and its
    /// files removed while it runs. `sleep` returns at once, to skip the wait.
    #[test]
    fn a_refused_signal_is_not_an_exit() {
        use crate::transport::protocol::{parse_kill, KillOutcome};

        let dir = scratch_path("shell-refusing");
        let arg = dir.to_string_lossy().into_owned();
        let mut held = spawn_standin(&format!("{arg}/aaaaaaaa.sock"), &[]);
        let script = format!("kill() {{ return 1; }}\nsleep() {{ :; }}\n{KILL_SCRIPT}");
        let out = run_script(&script, &[&arg, "aaaaaaaa", ""]);
        let _ = held.kill();
        let _ = held.wait();

        let outcome = parse_kill(&out.stdout);
        assert!(
            matches!(outcome, Ok(KillOutcome::Orphaned)),
            "{outcome:?}: {out:?}"
        );
    }

    /// The tests run as root play [`US`] and someone else, [`THEM`].
    const US: u32 = 65533;
    const THEM: u32 = 65534;

    /// Can this test process run others as [`US`] and [`THEM`]? Only root
    /// can, and only where its user namespace maps both.
    fn can_run_as_others() -> bool {
        use std::os::unix::process::CommandExt;

        let can = [US, THEM].into_iter().all(|uid| {
            Command::new("/bin/sh")
                .args(["-c", ":"])
                .uid(uid)
                .gid(uid)
                .current_dir("/")
                .status()
                .is_ok_and(|status| status.success())
        });
        if !can {
            eprintln!("skipping: needs root, to run processes as other users");
        }
        can
    }

    /// [`run_script`], run as `uid`.
    fn run_script_as(uid: u32, script: &str, args: &[&str]) -> Output {
        use std::os::unix::process::CommandExt;

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-s").args(args).uid(uid).gid(uid).current_dir("/");
        crate::proc::run_feeding_stdin(&mut cmd, script).expect("run the script")
    }

    /// [`scratch_dir`] in /tmp, as nvmux's own runtime directories are: [`US`]
    /// may not enter the temp dir, which can be private to root.
    fn scratch_dir_in_tmp(tag: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::path::Path::new("/tmp").join(format!("nvmux-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("scratch dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("chmod scratch dir");
        dir
    }

    #[test]
    fn a_dead_session_another_user_names_is_swept() {
        if !can_run_as_others() {
            return;
        }
        let dir = scratch_dir("shell-named");
        let sock = dir.join("aaaaaaaa.sock");
        crate::test_support::dead_socket(&sock);
        let path = sock.to_string_lossy().into_owned();
        let mut theirs = spawn_standin_as(Some(THEM), &path, &[]);
        let looked = serving_pid_answers(&path, "shell-named-bin");
        let out = run_script(LIST_SCRIPT, &[&dir.to_string_lossy()]);
        let swept = !sock.exists();
        let _ = theirs.kill();
        let _ = theirs.wait();
        let _ = std::fs::remove_dir_all(&dir);

        for (how, found) in looked {
            assert_eq!(found, "", "{how} took uid {THEM}'s process for the session");
        }
        let rows = crate::transport::protocol::parse_listing(&out.stdout).expect("a listing");
        assert!(
            rows.iter().all(|row| !row.pid_alive) && swept,
            "uid {THEM}'s process kept a dead session: {:?}",
            out.stdout
        );
    }

    /// Listed first, another user's process naming our socket would be the one
    /// signalled: as root that ends it, and as anyone else the session runs on.
    #[test]
    fn a_kill_ends_our_session_and_no_process_of_another_user() {
        use crate::transport::protocol::{parse_kill, KillOutcome};

        if !can_run_as_others() {
            return;
        }
        for us in [US, 0] {
            let dir = scratch_dir_in_tmp(&format!("shell-kill-as-{us}"));
            std::os::unix::fs::chown(&dir, Some(us), Some(us)).expect("give the directory away");
            std::fs::write(dir.join("aaaaaaaa.json"), "{}").expect("seed metadata");
            let sock = dir.join("aaaaaaaa.sock").to_string_lossy().into_owned();
            // First, so `ps` lists it first.
            let mut theirs = spawn_standin_as(Some(THEM), &sock, &[]);
            let mut ours = spawn_standin_as(Some(us), &sock, &[]);
            let out = run_script_as(us, KILL_SCRIPT, &[&dir.to_string_lossy(), "aaaaaaaa", ""]);
            let ended = crate::test_support::wait_until(std::time::Duration::from_secs(3), || {
                matches!(ours.try_wait(), Ok(Some(_)))
            });
            let spared = matches!(theirs.try_wait(), Ok(None));
            let kept = dir.join("aaaaaaaa.json").exists();
            for child in [&mut ours, &mut theirs] {
                let _ = child.kill();
                let _ = child.wait();
            }
            let _ = std::fs::remove_dir_all(&dir);

            let outcome = parse_kill(&out.stdout);
            assert!(
                matches!(outcome, Ok(KillOutcome::Killed)),
                "as uid {us}: {outcome:?}"
            );
            assert!(ended, "as uid {us}: the session was left running");
            assert!(spared, "as uid {us}: uid {THEM}'s process was ended");
            assert!(!kept, "as uid {us}: the session's files were kept");
        }
    }

    /// [`a_refused_signal_is_not_an_exit`] with the kernel refusing: `kill.sh`
    /// runs as [`US`], and `id` claims [`THEM`]'s uid so the lookup takes
    /// their process.
    #[test]
    fn a_process_that_refuses_our_signals_is_not_gone() {
        use crate::transport::protocol::{parse_kill, KillOutcome};

        if !can_run_as_others() {
            return;
        }
        // Missing, as `kill.sh` refuses a runtime directory `id` disowns.
        let dir = scratch_path("shell-refused");
        let arg = dir.to_string_lossy().into_owned();
        let mut theirs = spawn_standin_as(Some(THEM), &format!("{arg}/aaaaaaaa.sock"), &[]);
        let script = format!("id() {{ echo {THEM}; }}\nsleep() {{ :; }}\n{KILL_SCRIPT}");
        let out = run_script_as(US, &script, &[&arg, "aaaaaaaa", ""]);
        let _ = theirs.kill();
        let _ = theirs.wait();

        let outcome = parse_kill(&out.stdout);
        assert!(
            matches!(outcome, Ok(KillOutcome::Orphaned)),
            "{outcome:?}: {out:?}"
        );
    }

    /// A bound socket no command line names: only the kernel's list can tell,
    /// and missing it would delete a running session's files. Linux only
    /// (`/proc/net/unix`); elsewhere such a socket reads as stale.
    #[test]
    fn a_bound_socket_is_a_live_session_even_with_no_process_to_find() {
        if !std::path::Path::new("/proc/net/unix").exists() {
            eprintln!("skipping: this host does not keep /proc/net/unix");
            return;
        }
        let dir = scratch_dir("shell-bound");
        let sock = dir.join("aaaaaaaa.sock");
        // No command line names this path.
        let held = std::os::unix::net::UnixListener::bind(&sock).expect("bind");

        let out = run_script(LIST_SCRIPT, &[&dir.to_string_lossy()]);
        let rows = crate::transport::protocol::parse_listing(&out.stdout).expect("a listing");
        let survived = sock.exists();
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            rows.iter().any(|r| r.id == "aaaaaaaa" && r.pid_alive),
            "a bound socket is a live session: {:?}",
            out.stdout
        );
        assert!(survived, "and its files must not be swept");
    }

    /// `spawn.sh` writes the nested-launch marker that `crate::nested` reads;
    /// a rename on one side would silently stop catching nested launches.
    #[test]
    fn the_spawn_script_exports_the_marker_the_guard_reads() {
        let export = format!("export {}=", crate::nested::MARKER);
        assert!(
            SPAWN_SCRIPT.contains(&export),
            "spawn.sh must `{export}...` for the guard to find anything"
        );
    }

    /// Sessions inherit the umask nvmux was launched with; a mask set in
    /// `spawn.sh` would silently make their files more private than asked.
    /// Matched at line start so the script's comment naming it does not count.
    #[test]
    fn the_spawn_script_imposes_no_umask_on_the_session() {
        assert!(
            !SPAWN_SCRIPT.contains("\numask "),
            "spawn.sh must leave the caller's umask alone"
        );
    }

    /// `chmod 600` protects the socket, along with the 0700 directory
    /// `spawn.sh` requires (see `paths::ensure_dir_secure`).
    #[test]
    fn the_spawn_script_makes_the_socket_private_itself() {
        assert!(
            SPAWN_SCRIPT.contains(r#"chmod 600 "$sock""#),
            "spawn.sh must chmod the socket it waits for"
        );
    }

    #[test]
    fn the_spawn_script_runs_the_command_it_is_given() {
        let dir = scratch_path("shell-spawn");
        let sock = dir.join("aaaaaaaa.sock");

        // The stand-in binds nothing, so `SOCK` times out; the pid is only
        // reported once the script confirms the process serves this socket.
        let out = run_script(
            SPAWN_SCRIPT,
            &[
                &dir.to_string_lossy(),
                "aaaaaaaa",
                "/",
                "/bin/sh",
                "-c",
                STANDIN,
                "nvim",
                "--headless",
                "--listen",
                &sock.to_string_lossy(),
            ],
        );
        let spawned = crate::transport::protocol::parse_spawn(&out.stdout).expect("a spawn");
        let pid = spawned
            .pid
            .unwrap_or_else(|| panic!("the script must have run our command: {:?}", out.stdout));
        let seen = cmdline_of(pid);
        assert!(
            seen.contains(STANDIN),
            "pid {pid} is not the command we asked for: {seen:?}"
        );

        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Must fail at once, not wait 5 s for a socket that cannot appear.
    #[test]
    fn a_command_that_is_not_there_is_refused_rather_than_waited_for() {
        let dir = scratch_path("shell-spawn-missing");

        let started = std::time::Instant::now();
        let out = run_script(
            SPAWN_SCRIPT,
            &[
                &dir.to_string_lossy(),
                "aaaaaaaa",
                "/",
                "nvmux-no-such-editor",
                "--listen",
                "/tmp/x.sock",
            ],
        );
        let err = crate::transport::protocol::parse_spawn(&out.stdout)
            .expect_err("a missing command is not a spawn");
        assert!(
            err.to_string().contains("nvmux-no-such-editor")
                && err.to_string().contains("not found"),
            "the refusal must name the command: {err}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "it must not wait for a socket that cannot appear"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_prepended_script_does_not_source_the_prelude_again() {
        let out = run_script(LIST_SCRIPT, &["/nonexistent-runtime-dir"]);
        assert!(
            !out.stderr.contains("_prelude.sh"),
            "a prepended script tried to source the prelude: {}",
            out.stderr
        );
        assert!(
            out.stdout.contains("NVMUX_END"),
            "stdout: {:?} stderr: {}",
            out.stdout,
            out.stderr
        );
    }

    /// `flatten` must match the `tr -d '\n'` it replaced byte for byte,
    /// including pretty-printed files, CRLF, no trailing newline, and a `*`
    /// that must not glob. The tabs and CRs it keeps must still parse as JSON.
    #[test]
    fn flatten_matches_the_tr_it_replaced() {
        let dir = scratch_dir("shell-flatten");
        let cases: &[(&str, &str)] = &[
            ("plain", "{\"a\":1}"),
            ("pretty", "{\n  \"a\": 1\n}\n"),
            // Tab indentation only: a raw tab inside a string is invalid JSON
            // and is no longer deleted.
            ("tabbed", "{\n\t\"t\": \"a\"\n}\n"),
            ("crlf", "{\r\n\"a\": 1\r\n}\r\n"),
            ("no_newline", "{\"trail\":1}"),
            ("globby", "{\"p\":\"/x/*/y\"}"),
            ("backslash", "{\"b\":\"a\\\\b\"}"),
            ("empty", ""),
            ("newlines_only", "\n\n\n"),
            ("spaces", "{\"s\":\"  kept  \"}\n"),
        ];
        for (name, body) in cases {
            let path = dir.join(format!("{name}.json"));
            std::fs::write(&path, body).expect("write case");
            let arg = path.to_string_lossy().into_owned();
            let script = with_prelude!(r#"flatten "$1"; printf '%s' "$nvmux_flat""#, "\n");
            let flattened = run_script(script, &[&arg]);
            let tr = run_script(r#"tr -d '\n' < "$1""#, &[&arg]);
            assert!(flattened.ok(), "{name}: {}", flattened.stderr);
            assert_eq!(
                flattened.stdout, tr.stdout,
                "{name}: flatten and tr disagree"
            );
            assert!(
                !flattened.stdout.contains('\n'),
                "{name}: a newline survived, which would split the record"
            );
            if body.trim_start().starts_with('{') {
                serde_json::from_str::<serde_json::Value>(&flattened.stdout)
                    .unwrap_or_else(|e| panic!("{name}: flattened JSON no longer parses: {e}"));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_are_present_and_posix_sh() {
        for &(name, body) in SCRIPTS {
            assert!(!body.trim().is_empty(), "{name} is empty");
            let code = code_only(body);
            // The host's /bin/sh may be dash (Debian, Ubuntu), and `sh -n`
            // misses most of these: to dash `[[` is an unknown command that
            // fails only at runtime. The last three are GNU/BSD `find`
            // extensions: `-perm /mode` is GNU-only, `-perm +mode` BSD-only,
            // and either makes a permission check silently pass elsewhere.
            for bashism in [
                "[[ ",
                " ]]",
                " == ",
                "local ",
                "function ",
                "$'",
                "&>",
                "<<<",
                "${!",
                "+=",
                "echo -e",
                "read -a",
                "-perm /",
                "-perm +",
                "-maxdepth",
            ] {
                assert!(
                    !code.contains(bashism),
                    "{name} contains the bashism {bashism:?}"
                );
            }
        }
    }

    #[test]
    fn the_bashism_scan_would_catch_a_real_bashism() {
        let bash = "#!/bin/sh\nif [[ -n \"$x\" ]]; then :; fi\n";
        let code = code_only(bash);
        assert!(
            code.contains("[[ "),
            "the scan must see code, not just comments"
        );
        // ...and must not fire on the portable construct it resembles.
        let posix = "#!/bin/sh\nsed -n 's/[[:space:]]*//p'\n";
        assert!(!code_only(posix).contains("[[ "));
        assert!(!code_only(posix).contains(" ]]"));
    }

    fn syntax_check(text: &str) -> Output {
        crate::proc::run_feeding_stdin(Command::new("/bin/sh").arg("-n"), text).expect("run sh -n")
    }

    /// Checked as shipped and as framed: in the shared shell a top-level
    /// parse error makes dash exit, so nothing after it runs.
    #[test]
    fn scripts_pass_shell_syntax_check() {
        for &(name, body) in SCRIPTS {
            let out = syntax_check(body);
            assert!(out.ok(), "{name} is not valid sh: {}", out.stderr);
            let framed = crate::proc::frame("NVMUX_MARK_secret_1", body, &["a", "it's"]);
            let out = syntax_check(
                std::str::from_utf8(&framed).expect("frame() builds UTF-8 from &str inputs"),
            );
            assert!(
                out.ok(),
                "{name} is not valid sh once framed for the shell: {}",
                out.stderr
            );
        }
    }
}
