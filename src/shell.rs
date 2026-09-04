//! POSIX shell quoting.
//!
//! Every command nvmux runs on a session host is text that some shell will
//! re-parse, so the quoting here is the difference between "session named
//! `it's mine`" working and arbitrary command execution. There is exactly one
//! primitive ([`sq`]) and everything else is built from it.
//!
//! # Why single quotes, always
//!
//! Inside POSIX single quotes *no* character is special — not `$`, not
//! backslash, not backtick, not newline. The only thing that can't appear is a
//! single quote itself, which we close, escape, and reopen around. That makes
//! [`sq`] total: it is correct for every possible byte string, so there is no
//! "but what about..." case to get wrong later. Double-quote escaping, by
//! contrast, has to enumerate `$`, `` ` ``, `\`, `!`, and shell-specific extras
//! — a blocklist, and blocklists rot.

/// Quote `s` so that a POSIX shell parses it back as exactly one word with
/// exactly these bytes.
///
/// The empty string becomes `''` — an unquoted empty string would vanish
/// entirely and shift every following argument left by one.
pub fn sq(s: &str) -> String {
    // Worst case is every byte being a quote, which expands 1 -> 4.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close the literal, emit an escaped quote outside it, reopen.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Wrap `script` so it runs under the session user's **login** shell.
///
/// # Why this is two nested layers and not one
///
/// `ssh host <words>` does not run `<words>` directly: sshd joins them with
/// spaces and feeds the result to the user's shell from `/etc/passwd`. We do
/// not control what that shell is, and it is not necessarily POSIX — `fish` and
/// `csh` are both common login shells and neither supports `${VAR:-default}`.
///
/// So the outermost layer uses only syntax that is common to sh, bash, zsh,
/// fish and csh: a command name plus single-quoted words. That gets us into a
/// known-POSIX `sh`, and only *there* do we expand `$SHELL` and re-exec into
/// the login shell:
///
/// ```text
/// sh -c 'exec "${SHELL:-/bin/bash}" -l -c '\''<script>'\'''
/// ```
///
/// The login shell (`-l`) is the point of the whole exercise: `ssh host cmd`
/// gets a non-login, non-interactive shell, which on most setups means the
/// user's `PATH` additions never run and `nvim` — let alone the language
/// servers it spawns — is not found.
///
/// `exec` avoids leaving a pointless `sh` parked as the parent for the life of
/// the command.
pub fn login_shell_command(script: &str) -> String {
    // Inner layer: POSIX sh expands ${SHELL:-/bin/bash} and hands `script` to
    // the login shell as a single -c argument.
    let inner = format!("exec \"${{SHELL:-/bin/bash}}\" -l -c {}", sq(script));
    // Outer layer: only single-quoted words, so any login shell can parse it.
    format!("sh -c {}", sq(&inner))
}

/// Render `words` as a single shell-safe command line.
pub fn join(words: &[&str]) -> String {
    words.iter().map(|w| sq(w)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn sq_wraps_plain_words() {
        assert_eq!(sq("dotfiles"), "'dotfiles'");
    }

    #[test]
    fn sq_preserves_empty_as_a_real_argument() {
        assert_eq!(sq(""), "''");
    }

    #[test]
    fn sq_escapes_single_quotes() {
        assert_eq!(sq("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn sq_neutralizes_shell_metacharacters() {
        // None of these may survive as syntax.
        for s in [
            "a b",
            "$HOME",
            "`id`",
            "$(id)",
            "a;rm -rf /",
            "a|b",
            "a&b",
            "a>b",
            "a\\b",
            "a\nb",
            "*",
            "~",
            "!",
            "\"",
        ] {
            let quoted = sq(s);
            assert!(quoted.starts_with('\'') && quoted.ends_with('\''));
            assert_round_trips(s);
        }
    }

    /// The only assertion that actually matters: hand the quoted form to a real
    /// `/bin/sh` and check the bytes that come back out.
    fn assert_round_trips(s: &str) {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf %s {}", sq(s)))
            .output()
            .expect("spawn /bin/sh");
        assert!(out.status.success(), "sh failed for {s:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            s,
            "round-trip mismatch for {s:?}"
        );
    }

    #[test]
    fn sq_round_trips_through_a_real_shell() {
        for s in [
            "",
            "plain",
            "with space",
            "it's",
            "it's a 'quoted' thing",
            "'",
            "''",
            "'''",
            r#"quote " and 'quote'"#,
            "$HOME/`id`/$(uname)",
            "back\\slash",
            "new\nline",
            "tab\there",
            "unicode: héllo 世界 🚀",
            "-rf",
            "--flag=value",
        ] {
            assert_round_trips(s);
        }
    }

    #[test]
    fn join_quotes_every_word() {
        assert_eq!(join(&["nvim", "--listen", "/tmp/a b.sock"]), "'nvim' '--listen' '/tmp/a b.sock'");
    }

    /// Exercise both nesting layers through a real shell: the outer word-split
    /// and the inner `-c`. A single mis-escape here shows up as the login shell
    /// executing fragments of a session name.
    #[test]
    fn login_shell_command_survives_both_layers() {
        for payload in [
            "plain",
            "has space",
            "it's",
            "nested 'quotes' inside",
            "$HOME and `id` and $(uname)",
            "semi;colon && and || pipes",
            "quote\" and back\\slash",
        ] {
            let script = format!("printf %s {}", sq(payload));
            let cmd = login_shell_command(&script);
            // Run it the way sshd would: hand the whole line to a shell.
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .expect("spawn /bin/sh");
            assert!(
                out.status.success(),
                "login_shell_command failed for {payload:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout),
                payload,
                "payload corrupted through nesting: {payload:?}\ncmd was: {cmd}"
            );
        }
    }

    #[test]
    fn login_shell_command_uses_a_login_shell() {
        // -l is what makes the user's PATH additions apply; assert it is there
        // rather than trusting the format string to stay correct.
        let cmd = login_shell_command("true");
        assert!(cmd.contains("-l"), "must request a login shell: {cmd}");
        assert!(cmd.starts_with("sh -c "), "outer layer must be plain sh: {cmd}");
        assert!(
            cmd.contains("SHELL:-/bin/bash"),
            "must fall back to bash when $SHELL is unset: {cmd}"
        );
    }

    /// Injection guard: a hostile session name must not be able to break out of
    /// either quoting layer and run a command of its own.
    #[test]
    fn login_shell_command_resists_injection() {
        let hostile = "x'; touch /tmp/nvmux-pwned-$$; echo '";
        let script = format!("printf %s {}", sq(hostile));
        let cmd = login_shell_command(&script);
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(&cmd)
            .output()
            .expect("spawn /bin/sh");
        assert_eq!(String::from_utf8_lossy(&out.stdout), hostile);
        // If the payload had escaped, `touch` would have created a file and the
        // literal text would not have been echoed back verbatim.
    }
}
