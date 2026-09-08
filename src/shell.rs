//! Shell quoting, and the scripts that run on the session host.
//!
//! Spawning a remote session crosses **two** layers of shell: `ssh host cmd`
//! joins its arguments with spaces and the remote login shell parses them again.
//! Getting this wrong is a command injection, not a cosmetic bug.
//!
//! So no command *string* is ever built with interpolated values. Scripts are
//! fixed text delivered on stdin, and every variable part arrives as a
//! positional argument the remote shell never re-parses. [`quote`] covers the
//! one place a value must be embedded in a word — the `sh -s` argument list.

/// Single-quote a value so a POSIX shell reads it as exactly one literal word.
///
/// POSIX single quotes have no escape sequences, so the only case to handle is
/// the quote itself: close, insert an escaped quote, reopen.
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
        // Nothing a shell would look at twice; bare keeps logs readable.
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

/// Join arguments into a single shell word list.
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

/// Wrap a command so it runs under the user's **login** shell. Used by
/// [`crate::ssh::exec_args`] for every remote script run. `$SHELL` is
/// double-quoted so a value containing a space stays one word, and expands on
/// the remote side because the whole string travels as ssh's command.
pub fn login_shell_wrapper(inner: &str) -> String {
    format!(r#"exec "${{SHELL:-/bin/bash}}" -l -c {}"#, quote(inner))
}

/// Prepended to every script below, so what reaches the session host is one
/// self-contained program on stdin. A script run straight from a checkout
/// sources the same file instead — see `scripts/_prelude.sh`.
macro_rules! script {
    ($file:literal) => {
        concat!(include_str!("../scripts/_prelude.sh"), include_str!($file))
    };
}

/// Lists sessions on the host that owns them.
///
/// Batch-shaped on purpose: every session in one invocation, with liveness
/// already decided. Each `ssh` round trip costs ~230 ms even to localhost, so a
/// per-session API would feel fine locally and be unusable over a real link.
pub const LIST_SCRIPT: &str = script!("../scripts/list.sh");

pub const SPAWN_SCRIPT: &str = script!("../scripts/spawn.sh");

pub const KILL_SCRIPT: &str = script!("../scripts/kill.sh");

pub const PROBE_SCRIPT: &str = script!("../scripts/probe.sh");

/// Writes `<id>.json` on the session host. Used by the SSH transport, where
/// Rust cannot reach the file directly.
pub const WRITE_META_SCRIPT: &str = script!("../scripts/write_meta.sh");

/// Every script, for the tests that check all of them the same way.
#[cfg(test)]
const SCRIPTS: &[(&str, &str)] = &[
    ("list.sh", LIST_SCRIPT),
    ("spawn.sh", SPAWN_SCRIPT),
    ("kill.sh", KILL_SCRIPT),
    ("probe.sh", PROBE_SCRIPT),
    ("write_meta.sh", WRITE_META_SCRIPT),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Ask a real `/bin/sh` what it made of our quoting.
    ///
    /// The point of this helper is that the assertions below are not checking
    /// our idea of shell syntax against itself — they round-trip through the
    /// actual parser that will see these strings in production.
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

    /// The two-layer case: quoted once for `ssh`'s argv join, then parsed again
    /// by the remote shell. This is the failure the module docs describe.
    #[test]
    fn survives_two_layers_of_shell() {
        for original in ["my project", r#"a "quoted" name"#, "it's", "$(id)"] {
            let inner = format!("printf %s {}", quote(original));
            let outer = login_shell_wrapper(&inner);
            // `ssh host <args>` joins with spaces; emulate that faithfully.
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&outer)
                .output()
                .expect("run sh");
            assert!(
                out.status.success(),
                "outer layer failed for {original:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                original,
                "two-layer round trip failed for {original:?}\nouter: {outer}"
            );
        }
    }

    #[test]
    fn login_wrapper_uses_the_users_shell_with_bash_fallback() {
        let w = login_shell_wrapper("true");
        assert!(w.contains("${SHELL:-/bin/bash}"), "no fallback in {w}");
        assert!(w.contains(" -l -c "), "not a login shell in {w}");
        // $SHELL must be expanded remotely, so it must NOT be single-quoted.
        assert!(
            !w.contains(r"'${SHELL"),
            "SHELL was quoted and will not expand: {w}"
        );
    }

    /// Strip comments and POSIX character classes so the bashism scan below
    /// looks only at code.
    ///
    /// Both exclusions are load-bearing: `[[:space:]]` is a perfectly portable
    /// character class that contains `[[` and `]]`, and the word "local"
    /// appears in prose. Without this the scan rejects correct scripts.
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

    /// Every shipped script carries the prelude, and carries it once: a script
    /// delivered without it would die on the first `finish` with "not found".
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

    /// `scripts/spawn.sh` writes the nested-launch marker and `crate::nested`
    /// reads it. Two files, one name: renaming it on either side would stop
    /// every nested launch being caught, and nothing else would fail.
    #[test]
    fn the_spawn_script_exports_the_marker_the_guard_reads() {
        let export = format!("export {}=", crate::nested::MARKER);
        assert!(
            SPAWN_SCRIPT.contains(&export),
            "spawn.sh must `{export}...` for the guard to find anything"
        );
    }

    /// The guard the prelude sets is what stops a prepended script sourcing it
    /// a second time — and what lets the same file still run from a checkout.
    #[test]
    fn a_prepended_script_does_not_source_the_prelude_again() {
        let out = Command::new("/bin/sh")
            .arg("-s")
            .arg("/nonexistent-runtime-dir")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin
                    .take()
                    .expect("stdin")
                    .write_all(LIST_SCRIPT.as_bytes())?;
                c.wait_with_output()
            })
            .expect("run list.sh");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("_prelude.sh"),
            "a prepended script tried to source the prelude: {stderr}"
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("NVMUX_END"),
            "stdout: {:?} stderr: {stderr}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn scripts_are_present_and_posix_sh() {
        for &(name, body) in SCRIPTS {
            assert!(!body.trim().is_empty(), "{name} is empty");
            let code = code_only(body);
            // These run under whatever /bin/sh the session host has, which on
            // Debian and Ubuntu is dash.
            //
            // `sh -n` does not catch most of these: to dash, `[[` is simply an
            // unknown command name, so it parses fine and fails at runtime.
            // That is exactly the bug that works on the developer's mac (where
            // /bin/sh may be bash) and fails on the user's server.
            // The last three are GNU/BSD `find` extensions rather than
            // bashisms, caught here for the same reason: `-perm /mode` is
            // GNU-only and `-perm +mode` is BSD-only, and either one makes a
            // permission check silently pass on the other platform.
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

    /// The scan must actually reject bash, or it is decoration.
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

    #[test]
    fn scripts_pass_shell_syntax_check() {
        for &(name, body) in SCRIPTS {
            let out = Command::new("/bin/sh")
                .arg("-n")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut c| {
                    use std::io::Write;
                    c.stdin.take().expect("stdin").write_all(body.as_bytes())?;
                    c.wait_with_output()
                })
                .expect("run sh -n");
            assert!(
                out.status.success(),
                "{name} is not valid sh: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
