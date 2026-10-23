//! The command line a session's Neovim is launched with.
//!
//! It must contain [`PLACEHOLDER`] exactly once: listings and kills find a
//! session by `--listen <sock>` in its command line (`scripts/_prelude.sh`,
//! `scripts/kill.sh`). The line is split here and passed as separate
//! arguments (see [`crate::shell`]); no shell expands globs, `$VAR`, `~` or
//! `VAR=value` prefixes.

use crate::error::SessionError;

/// Replaced by the session's socket path, unknown until it has an id.
pub const PLACEHOLDER: &str = "{sock}";

/// Also the default of [`crate::config::SessionSettings`].
pub const DEFAULT: &str = "nvim --headless --listen {sock}";

/// Bounded like a name: the line reaches the terminal, logs and argv. Real
/// ones are about 30 characters.
const MAX_LEN: usize = 512;

/// The line as written (shown and stored) and its words (executed), split
/// once so the validated split is what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    line: String,
    words: Vec<String>,
}

impl Launch {
    pub fn parse(line: &str) -> Result<Self, SessionError> {
        let line = line.trim();
        let invalid = |reason| {
            Err(SessionError::InvalidCommand {
                command: line.to_string(),
                reason,
            })
        };

        if line.is_empty() {
            return invalid("must not be empty");
        }
        if line.len() > MAX_LEN {
            return invalid("must be 512 bytes or fewer");
        }
        // Same rule as session names: the line is drawn on the prompt and
        // must not smuggle escape sequences.
        if line.chars().any(crate::session::is_unrenderable) {
            return invalid("must not contain control or invisible formatting characters");
        }

        let words = split(line).map_err(|reason| SessionError::InvalidCommand {
            command: line.to_string(),
            reason,
        })?;

        let Some(first) = words.first() else {
            return invalid("must not be empty");
        };
        // With no shell, `FOO=1 nvim` would run a program named `FOO=1`;
        // refuse it here instead of a confusing "not found" at spawn.
        if is_assignment(first) {
            return invalid(
                "starts with a variable assignment, which nothing here expands — \
                 use `env NAME=value nvim …`",
            );
        }

        match words.iter().map(|w| w.matches(PLACEHOLDER).count()).sum() {
            1 => {}
            0 => {
                return invalid(
                    "must contain {sock} — it becomes the session's socket, \
                     which is how nvmux finds and kills it",
                )
            }
            _ => return invalid("must contain {sock} exactly once"),
        }

        Ok(Self {
            line: line.to_string(),
            words,
        })
    }

    pub fn line(&self) -> &str {
        &self.line
    }

    /// The words with [`PLACEHOLDER`] replaced by `sock`.
    pub fn argv_for(&self, sock: &str) -> Vec<String> {
        self.words
            .iter()
            .map(|w| w.replace(PLACEHOLDER, sock))
            .collect()
    }
}

/// Split into words like a POSIX shell, without expansion: `'…'` is literal,
/// `"…"` escapes only `\"` and `\\`. An unclosed quote or trailing backslash
/// is an error rather than a silently truncated word.
fn split(line: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '\'' {
                        closed = true;
                        break;
                    }
                    word.push(c);
                }
                if !closed {
                    return Err("has an unclosed ' quote");
                }
            }
            '"' => {
                started = true;
                let mut closed = false;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => {
                            closed = true;
                            break;
                        }
                        // As in a shell, any other backslash is literal.
                        '\\' => match chars.next() {
                            Some(escaped @ ('"' | '\\')) => word.push(escaped),
                            Some(other) => {
                                word.push('\\');
                                word.push(other);
                            }
                            None => return Err("ends with a dangling backslash"),
                        },
                        other => word.push(other),
                    }
                }
                if !closed {
                    return Err("has an unclosed \" quote");
                }
            }
            '\\' => {
                started = true;
                match chars.next() {
                    Some(escaped) => word.push(escaped),
                    None => return Err("ends with a dangling backslash"),
                }
            }
            other => {
                started = true;
                word.push(other);
            }
        }
    }

    if started {
        words.push(word);
    }
    Ok(words)
}

/// Whether `word` starts with a shell assignment (`NAME=`).
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        Launch::parse(line)
            .unwrap_or_else(|e| panic!("{line:?} should parse: {e}"))
            .argv_for("/tmp/s.sock")
    }

    fn reason(line: &str) -> String {
        match Launch::parse(line) {
            Err(SessionError::InvalidCommand { reason, .. }) => reason.to_string(),
            Err(other) => panic!("{line:?}: unexpected error {other:?}"),
            Ok(ok) => panic!("{line:?} should have been refused, got {ok:?}"),
        }
    }

    #[test]
    fn the_default_is_a_valid_command() {
        let l = Launch::parse(DEFAULT).expect("the built-in default must parse");
        assert_eq!(
            l.argv_for("/tmp/nvmux-501/abcdefgh.sock"),
            [
                "nvim",
                "--headless",
                "--listen",
                "/tmp/nvmux-501/abcdefgh.sock"
            ]
        );
    }

    #[test]
    fn splits_on_runs_of_whitespace() {
        assert_eq!(
            words("  nvim   --headless   --listen  {sock}  "),
            ["nvim", "--headless", "--listen", "/tmp/s.sock"]
        );
    }

    #[test]
    fn a_tab_is_a_control_character_not_a_separator() {
        assert!(reason("nvim\t--listen {sock}").contains("control"));
    }

    /// The stored line is shown back as the next hint.
    #[test]
    fn the_line_is_kept_verbatim_apart_from_the_edges() {
        let l = Launch::parse("  nvim  -u 'my init.lua' --listen {sock}  ").expect("parses");
        assert_eq!(l.line(), "nvim  -u 'my init.lua' --listen {sock}");
    }

    #[test]
    fn quoting_keeps_a_path_with_spaces_in_one_word() {
        let want = ["/opt/my nvim/bin/nvim", "--listen", "/tmp/s.sock"];
        assert_eq!(words(r#"'/opt/my nvim/bin/nvim' --listen {sock}"#), want);
        assert_eq!(words(r#""/opt/my nvim/bin/nvim" --listen {sock}"#), want);
        assert_eq!(words(r"/opt/my\ nvim/bin/nvim --listen {sock}"), want);
    }

    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            words(r"nvim -c 'let x = \n' {sock}"),
            ["nvim", "-c", r"let x = \n", "/tmp/s.sock"]
        );
    }

    /// `-c` snippets rely on other backslashes being kept.
    #[test]
    fn double_quotes_escape_only_themselves_and_a_backslash() {
        assert_eq!(
            words(r#"nvim -c "say \"hi\"" {sock}"#),
            ["nvim", "-c", r#"say "hi""#, "/tmp/s.sock"]
        );
        assert_eq!(
            words(r#"nvim -c "a\\b" {sock}"#),
            ["nvim", "-c", r"a\b", "/tmp/s.sock"]
        );
        assert_eq!(
            words(r#"nvim -c "a\nb" {sock}"#),
            ["nvim", "-c", r"a\nb", "/tmp/s.sock"]
        );
    }

    #[test]
    fn adjacent_quoted_and_bare_pieces_are_one_word() {
        assert_eq!(
            words(r#"nvim -u'a b'c --listen {sock}"#),
            ["nvim", "-ua bc", "--listen", "/tmp/s.sock"]
        );
    }

    #[test]
    fn an_empty_quoted_word_survives_as_an_empty_argument() {
        assert_eq!(words("nvim '' {sock}"), ["nvim", "", "/tmp/s.sock"]);
    }

    #[test]
    fn non_ascii_survives_the_split() {
        assert_eq!(
            words("nvim -u ~日本語/init.lua {sock}"),
            ["nvim", "-u", "~日本語/init.lua", "/tmp/s.sock"]
        );
    }

    #[test]
    fn an_unbalanced_quote_is_refused_rather_than_truncated() {
        assert!(reason("nvim -c 'unclosed {sock}").contains("unclosed '"));
        assert!(reason(r#"nvim -c "unclosed {sock}"#).contains("unclosed \""));
        assert!(reason(r"nvim {sock} \").contains("dangling backslash"));
        assert!(reason(r#"nvim {sock} "a\"#).contains("dangling backslash"));
    }

    #[test]
    fn the_socket_placeholder_is_mandatory_and_singular() {
        assert!(reason("nvim --headless").contains("{sock}"));
        assert!(reason("nvim --listen {sock} --listen {sock}").contains("exactly once"));
        // Quoting does not matter: the executed word is what counts.
        Launch::parse("nvim --listen '{sock}'").expect("a quoted placeholder is still one");
    }

    #[test]
    fn the_placeholder_is_replaced_only_where_it_appears() {
        let l = Launch::parse("nvim --listen {sock} --cmd 'set title'").expect("parses");
        assert_eq!(
            l.argv_for("/tmp/x.sock"),
            ["nvim", "--listen", "/tmp/x.sock", "--cmd", "set title"]
        );
    }

    #[test]
    fn the_placeholder_may_be_part_of_a_larger_word() {
        assert_eq!(
            words("nvim --listen={sock}"),
            ["nvim", "--listen=/tmp/s.sock"]
        );
    }

    #[test]
    fn a_leading_assignment_is_refused_with_the_remedy() {
        let r = reason("NVIM_APPNAME=work nvim --listen {sock}");
        assert!(r.contains("env "), "{r}");
        // Only at the start, and only when it really is an assignment.
        Launch::parse("env NVIM_APPNAME=work nvim --listen {sock}").expect("env is the way");
        Launch::parse("nvim --cmd let\\ x=1 --listen {sock}").expect("not the first word");
        Launch::parse("./=weird --listen {sock}").expect("not an assignment shape");
        Launch::parse("1FOO=x --listen {sock}").expect("no shell name starts with a digit");
    }

    #[test]
    fn an_empty_command_is_refused() {
        for line in ["", "   ", "\t"] {
            assert!(reason(line).contains("empty"), "{line:?}");
        }
    }

    #[test]
    fn a_command_that_would_corrupt_the_display_is_refused() {
        assert!(reason("nvim\x1b[31m {sock}").contains("control"));
        assert!(reason("nvim\u{202E} {sock}").contains("control"));
    }

    #[test]
    fn an_absurdly_long_command_is_refused() {
        let long = format!("nvim {} {}", "x".repeat(MAX_LEN), PLACEHOLDER);
        assert!(reason(&long).contains("512 bytes"));
        let ok = format!("nvim {} {}", "x".repeat(MAX_LEN - 20), PLACEHOLDER);
        Launch::parse(&ok).expect("just inside the cap");
    }

    #[test]
    fn nothing_is_expanded() {
        assert_eq!(
            words("nvim -u ~/init.lua {sock}"),
            ["nvim", "-u", "~/init.lua", "/tmp/s.sock"]
        );
        assert_eq!(
            words("nvim -u $HOME/init.lua {sock}"),
            ["nvim", "-u", "$HOME/init.lua", "/tmp/s.sock"]
        );
        assert_eq!(
            words("nvim -u *.lua {sock}"),
            ["nvim", "-u", "*.lua", "/tmp/s.sock"]
        );
        // No shell operators either.
        assert_eq!(
            words("nvim {sock} ; rm -rf /"),
            ["nvim", "/tmp/s.sock", ";", "rm", "-rf", "/"]
        );
    }
}
