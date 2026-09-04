//! Shell quoting, and the scripts that run on the session host.
//!
//! # Why this needs its own module and real tests
//!
//! Spawning a remote session goes through **two** layers of shell. `ssh host cmd`
//! joins its arguments with spaces and hands the result to the remote login
//! shell, which parses it again. A session named `my "project"` therefore has to
//! survive being quoted, transported, and re-parsed. Getting this wrong is a
//! command injection, not a cosmetic bug.
//!
//! The approach is to never build a command *string* with interpolated values.
//! Scripts are fixed text delivered on stdin, and every variable part arrives as
//! a positional argument (`$1`, `$2`, ...) that the remote shell never re-parses.
//! [`quote`] exists for the one place a value must be embedded in a word — the
//! argument list handed to `sh -s`.

/// Single-quote a value so a POSIX shell reads it as exactly one literal word.
///
/// POSIX single quotes have no escape sequences at all, so the only thing that
/// needs handling is the quote itself: close, insert an escaped quote, reopen.
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
        // Nothing a shell would look at twice; leave it bare so log lines and
        // error messages stay readable.
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

/// Wrap a command so it runs under the user's **login** shell.
///
/// `ssh host cmd` gives a non-login, non-interactive shell. On most real setups
/// that means `nvim` and every language server are not on `$PATH`, because they
/// were put there by `.zprofile` / `.bash_profile`, which only a login shell
/// reads. Sessions would fail to spawn with a confusing "command not found".
///
/// `$SHELL` is expanded on the *remote* side, so it is the remote user's shell,
/// with `bash -l` as the fallback when it is unset (cron-like environments, some
/// container images).
pub fn login_shell_wrapper(inner: &str) -> String {
    format!(r#"exec "${{SHELL:-/bin/bash}}" -l -c {}"#, quote(inner))
}

/// Lists sessions on the host that owns them.
///
/// Batch-shaped on purpose: it returns every session in one invocation, with
/// liveness already decided. Each `ssh` round trip costs ~230 ms even to
/// localhost, so a per-session API would be unusable remotely while feeling
/// perfectly fine in local testing.
pub const LIST_SCRIPT: &str = include_str!("../scripts/list.sh");

/// Spawns a detached headless nvim. Wired up in milestone 2.
pub const SPAWN_SCRIPT: &str = include_str!("../scripts/spawn.sh");

/// Terminates a session and removes its files. Wired up in milestone 2.
pub const KILL_SCRIPT: &str = include_str!("../scripts/kill.sh");

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
        // If quoting were wrong, the subshell would run and the output would
        // differ from the literal text.
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

    #[test]
    fn scripts_are_present_and_posix_sh() {
        for (name, body) in [
            ("list.sh", LIST_SCRIPT),
            ("spawn.sh", SPAWN_SCRIPT),
            ("kill.sh", KILL_SCRIPT),
        ] {
            assert!(!body.trim().is_empty(), "{name} is empty");
            let code = code_only(body);
            // These run under whatever /bin/sh the session host has, which on
            // Debian and Ubuntu is dash.
            //
            // `sh -n` does not catch most of these: to dash, `[[` is simply an
            // unknown command name, so it parses fine and fails at runtime.
            // That is exactly the bug that works on the developer's mac (where
            // /bin/sh may be bash) and fails on the user's server.
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
        for (name, body) in [
            ("list.sh", LIST_SCRIPT),
            ("spawn.sh", SPAWN_SCRIPT),
            ("kill.sh", KILL_SCRIPT),
        ] {
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
