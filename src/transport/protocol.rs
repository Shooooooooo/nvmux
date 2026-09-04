//! Pure parsing and argument building for the session-host scripts.
//!
//! Zero I/O lives here, which is the point: this is the code most likely to
//! break silently (a changed field order, an empty listing that should have
//! been an error), so it is the code that gets the heaviest tests.
//!
//! If `if self.is_remote()` ever appears in this file, the abstraction has
//! leaked — that condition belongs inside a script or inside
//! [`crate::transport::Transport::local_socket_for`].

use crate::error::{NvmuxError, Result};
use crate::session::{Liveness, Session};

/// The sentinel `list.sh` prints after the last record.
const TERMINATOR: &str = "NVMUX_END";

/// One row as the script reported it, before liveness is probed properly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub id: String,
    /// The script's cheap `kill -0` hint. A hint only: pids are reused, and a
    /// live pid says nothing about whether nvim is serving its socket.
    pub pid_alive: bool,
    /// Raw `<id>.json` contents, or empty if the file was missing.
    pub json: String,
}

/// Parse the output of `list.sh`.
///
/// # Why a missing terminator is an error
///
/// `ssh -n` combined with `sh -s` produces a silently **empty** result: stdin
/// comes from `/dev/null`, `sh` reads an empty script and exits 0. Without the
/// sentinel that is indistinguishable from "this host has no sessions", and the
/// picker would show an empty list instead of reporting a broken connection.
pub fn parse_listing(stdout: &str) -> Result<Vec<Listed>> {
    let mut rows = Vec::new();
    let mut terminated = false;

    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line == TERMINATOR {
            terminated = true;
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        match fields.next() {
            Some("S") => {}
            // Anything else is the remote shell talking (a profile that prints a
            // banner, a warning). Ignore it rather than failing: shells print
            // things, and refusing to list sessions because of a MOTD would be
            // maddening.
            _ => {
                tracing::debug!(line, "ignoring unrecognised line from list.sh");
                continue;
            }
        }
        let (Some(id), Some(alive)) = (fields.next(), fields.next()) else {
            tracing::debug!(line, "ignoring truncated record from list.sh");
            continue;
        };
        rows.push(Listed {
            id: id.to_string(),
            pid_alive: alive == "1",
            json: fields.next().unwrap_or("").to_string(),
        });
    }

    if !terminated {
        return Err(NvmuxError::Session(crate::error::SessionError::NotFound(
            format!(
                "the session listing did not complete (no {TERMINATOR} marker). \
                 The command may not have run at all."
            ),
        )));
    }
    Ok(rows)
}

/// Turn parsed rows into sessions, dropping ones whose metadata is unusable.
///
/// A socket with no readable metadata is reported rather than silently hidden:
/// it means an interrupted create, and the user should be able to see and kill
/// it. It gets a placeholder name derived from its id.
pub fn rows_to_sessions(rows: Vec<Listed>) -> Vec<Session> {
    rows.into_iter()
        .map(|row| {
            let mut session = if row.json.trim().is_empty() {
                orphan(&row.id)
            } else {
                match Session::from_json(row.json.as_bytes(), std::path::Path::new("<listing>")) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(id = %row.id, error = %e, "unreadable session metadata");
                        orphan(&row.id)
                    }
                }
            };
            // The script's hint is the starting point; a real probe overwrites it.
            session.state.liveness = if row.pid_alive {
                Liveness::Busy
            } else {
                Liveness::Dead
            };
            session
        })
        .collect()
}

fn orphan(id: &str) -> Session {
    let mut s = Session::new(id.to_string(), format!("<orphan {id}>"), 0);
    s.created = 0;
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(body: &str) -> String {
        format!("{body}NVMUX_END\n")
    }

    #[test]
    fn parses_a_normal_listing() {
        let out = listing(
            "S\tabcdefgh\t1\t{\"id\":\"abcdefgh\",\"name\":\"dotfiles\",\"created\":1,\"pid\":42}\n\
             S\tijklmnop\t0\t{\"id\":\"ijklmnop\",\"name\":\"scratch\",\"created\":2,\"pid\":43}\n",
        );
        let rows = parse_listing(&out).expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "abcdefgh");
        assert!(rows[0].pid_alive);
        assert!(!rows[1].pid_alive);

        let sessions = rows_to_sessions(rows);
        assert_eq!(sessions[0].name, "dotfiles");
        assert_eq!(sessions[1].name, "scratch");
        assert_eq!(sessions[1].state.liveness, Liveness::Dead);
    }

    #[test]
    fn an_empty_but_terminated_listing_is_no_sessions() {
        assert!(parse_listing("NVMUX_END\n").expect("parse").is_empty());
    }

    /// The `ssh -n` trap: empty output, exit 0, and it must not read as success.
    #[test]
    fn output_with_no_terminator_is_an_error() {
        assert!(
            parse_listing("").is_err(),
            "empty output must not parse as zero sessions"
        );
        assert!(
            parse_listing("S\tabcdefgh\t1\t{}\n").is_err(),
            "truncated listing must error"
        );
    }

    #[test]
    fn shell_noise_before_the_records_is_ignored() {
        // Login shells print things: MOTDs, `nvm` banners, warnings on stdout.
        let out = format!(
            "Welcome to Ubuntu!\n\
             bash: warning: setlocale: LC_ALL: cannot change locale\n\
             {}",
            listing(
                "S\tabcdefgh\t1\t{\"id\":\"abcdefgh\",\"name\":\"x\",\"created\":1,\"pid\":1}\n"
            )
        );
        let rows = parse_listing(&out).expect("parse");
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_socket_without_metadata_is_surfaced_not_hidden() {
        let rows = parse_listing(&listing("S\tabcdefgh\t0\t\n")).expect("parse");
        let sessions = rows_to_sessions(rows);
        assert_eq!(sessions.len(), 1, "an orphaned socket must still be listed");
        assert!(sessions[0].name.contains("abcdefgh"));
    }

    #[test]
    fn corrupt_metadata_does_not_lose_the_whole_listing() {
        let out = listing(
            "S\tabcdefgh\t1\t{not json at all\n\
             S\tijklmnop\t1\t{\"id\":\"ijklmnop\",\"name\":\"ok\",\"created\":1,\"pid\":2}\n",
        );
        let sessions = rows_to_sessions(parse_listing(&out).expect("parse"));
        assert_eq!(sessions.len(), 2, "one bad row must not drop the good one");
        assert_eq!(sessions[1].name, "ok");
    }

    #[test]
    fn names_with_tabs_cannot_break_the_record_format() {
        // `splitn(4)` means the json field keeps any tabs it contains rather
        // than shifting later fields. list.sh strips tabs too, belt and braces.
        let out = listing(
            "S\tabcdefgh\t1\t{\"id\":\"abcdefgh\",\"name\":\"a\tb\",\"created\":1,\"pid\":2}\n",
        );
        let rows = parse_listing(&out).expect("parse");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].json.contains("a\tb"));
    }

    #[test]
    fn carriage_returns_from_a_pty_are_tolerated() {
        let out = "S\tabcdefgh\t1\t{\"id\":\"abcdefgh\",\"name\":\"x\",\"created\":1,\"pid\":2}\r\nNVMUX_END\r\n";
        assert_eq!(parse_listing(out).expect("parse").len(), 1);
    }
}
