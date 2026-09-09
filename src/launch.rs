//! The command a session's Neovim is launched with.
//!
//! One line, typed by the user or read from the config, that says how to start
//! the editor a session *is*. The socket it must listen on is not known when the
//! line is written, so the line carries a [`PLACEHOLDER`] which nvmux replaces
//! with the real path at spawn time:
//!
//! ```text
//! nvim --headless --listen {sock}
//! └─ words, split here in Rust      └─ becomes /tmp/nvmux-501/abcdefgh.sock
//! ```
//!
//! # Why the socket is mandatory
//!
//! `{sock}` is not decoration. Every later listing and every kill finds a
//! session by looking for `--listen <sock>` in a process's command line
//! (`scripts/_prelude.sh`, `scripts/kill.sh`), and `nvim --listen` is what binds
//! the socket the picker attaches through. A line without it would start an
//! editor nvmux could neither reach nor stop, so a missing placeholder is
//! refused rather than quietly appended to.
//!
//! # Split, never evaluated
//!
//! The line is split into words *here* and travels to the session host as
//! separate positional arguments, exactly like every other variable part — see
//! [`crate::shell`], which explains why no command string is ever built by
//! interpolation. Nothing is *expanded*: there are no globs, no `$VAR`, no `~`,
//! and no `VAR=value` prefixes, because there is no shell in the path that would
//! honour them. `env` covers the last of those and the errors say so.

use crate::error::SessionError;

/// Stands in for the session's socket path, which is not known until the
/// session has an id.
pub const PLACEHOLDER: &str = "{sock}";

/// What nvmux launches when nothing says otherwise. The one place this is
/// written down; [`crate::config::SessionSettings`] defaults to it.
pub const DEFAULT: &str = "nvim --headless --listen {sock}";

/// A line is metadata, but it reaches a terminal, a log and a shell argument
/// list, so it is bounded like a name is. Generous: a real one runs to about
/// thirty characters.
const MAX_LEN: usize = 512;

/// A validated launch command: the line as it was written, and the words it
/// splits into.
///
/// Holding both is the point. The line is what is shown, remembered and stored;
/// the words are what is executed, and deriving them once means the split that
/// was validated is the split that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    line: String,
    words: Vec<String>,
}

impl Launch {
    /// Validate a line and split it. Every rejection names what is wrong with
    /// it, because this is typed at a prompt and edited by hand in a config.
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
        // The same rule session names live by, and for the same reasons: the
        // line is drawn on the prompt and could otherwise smuggle escape
        // sequences into the terminal.
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
        // There is no shell to honour an assignment, so `FOO=1 nvim` would look
        // for a program called `FOO=1`. Caught here rather than as a baffling
        // "not found" from the spawn script.
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

    /// The line as it was written — what is shown, remembered and stored.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// The words to run, with the placeholder replaced by a real socket path.
    ///
    /// `parse` has already established there is exactly one, so this cannot
    /// silently produce a command line with none.
    pub fn argv_for(&self, sock: &str) -> Vec<String> {
        self.words
            .iter()
            .map(|w| w.replace(PLACEHOLDER, sock))
            .collect()
    }
}

/// Split a line the way a POSIX shell splits words, and no further.
///
/// Quoting is honoured — `'…'` literally, `"…"` with `\"` and `\\` — so a path
/// with a space in it is one word. Nothing is expanded. An unterminated quote or
/// a trailing backslash is an error rather than a word that quietly loses its
/// tail: the alternative is a session started with something the user did not
/// write.
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
                        // Only these two: a backslash before anything else is
                        // literal inside double quotes, as in a real shell.
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

/// `NAME=` at the start of a word, as a shell would read an assignment.
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

    /// A tab is whitespace to a shell but a control character here, and the
    /// second rule wins: this line is drawn on the prompt.
    #[test]
    fn a_tab_is_a_control_character_not_a_separator() {
        assert!(reason("nvim\t--listen {sock}").contains("control"));
    }

    /// The line is stored as it was written, not as it was split — that is what
    /// gets shown back as the next hint.
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

    /// Single quotes have no escapes at all in a POSIX shell, and neither here.
    #[test]
    fn single_quotes_are_literal() {
        assert_eq!(
            words(r"nvim -c 'let x = \n' {sock}"),
            ["nvim", "-c", r"let x = \n", "/tmp/s.sock"]
        );
    }

    /// Inside double quotes only `\"` and `\\` are escapes; anything else keeps
    /// its backslash, which is what a shell does and what a Lua or Vim snippet
    /// passed with `-c` depends on.
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

    /// A word that quietly loses its tail would start a session with something
    /// the user did not write.
    #[test]
    fn an_unbalanced_quote_is_refused_rather_than_truncated() {
        assert!(reason("nvim -c 'unclosed {sock}").contains("unclosed '"));
        assert!(reason(r#"nvim -c "unclosed {sock}"#).contains("unclosed \""));
        assert!(reason(r"nvim {sock} \").contains("dangling backslash"));
        assert!(reason(r#"nvim {sock} "a\"#).contains("dangling backslash"));
    }

    /// Without it nvmux starts an editor it can neither find nor kill.
    #[test]
    fn the_socket_placeholder_is_mandatory_and_singular() {
        assert!(reason("nvim --headless").contains("{sock}"));
        assert!(reason("nvim --listen {sock} --listen {sock}").contains("exactly once"));
        // Quoted still counts: it is the word that is executed that matters.
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

    /// The placeholder need not be a word of its own — a wrapper may want to
    /// glue it to something.
    #[test]
    fn the_placeholder_may_be_part_of_a_larger_word() {
        assert_eq!(
            words("nvim --listen={sock}"),
            ["nvim", "--listen=/tmp/s.sock"]
        );
    }

    /// There is no shell here, so `FOO=1 nvim` would look for a program called
    /// `FOO=1`. Say what to do instead rather than letting the spawn fail.
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

    /// Whatever a shell would have done with these, nvmux does not: they reach
    /// the editor as the literal words they are.
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
        // And nothing here is a shell operator either.
        assert_eq!(
            words("nvim {sock} ; rm -rf /"),
            ["nvim", "/tmp/s.sock", ";", "rm", "-rf", "/"]
        );
    }
}
