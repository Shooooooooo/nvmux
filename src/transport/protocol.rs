//! Pure parsing and argument building for the session-host scripts. Zero I/O,
//! so the code most likely to break silently is the code with the heaviest
//! tests.
//!
//! If `if self.is_remote()` ever appears here the abstraction has leaked — that
//! belongs in a script or in [`crate::transport::Transport::local_socket_for`].

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
            // The remote shell talking — a profile banner, a warning. Refusing
            // to list sessions because of a MOTD would be maddening.
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

/// What `spawn.sh` reported.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spawned {
    /// The pid of the new nvim, **validated** against the socket it serves.
    /// `None` is not fatal, but nvmux must never signal an unvalidated pid:
    /// pids are reused, and the number could name anything by now.
    pub pid: Option<u32>,
    /// Whether the listen socket appeared before the script gave up waiting.
    pub socket_appeared: bool,
}

/// Parse the output of `spawn.sh`.
pub fn parse_spawn(stdout: &str) -> Result<Spawned> {
    let mut out = Spawned::default();
    let mut terminated = false;
    let mut error = None;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        match line.split_once(' ') {
            Some(("PID", v)) => out.pid = v.trim().parse().ok().filter(|p| *p > 1),
            Some(("SOCK", v)) => out.socket_appeared = v.trim() == "ok",
            // Surfacing the script's own reason beats a generic timeout.
            Some(("ERROR", v)) => error = Some(v.trim().to_string()),
            _ if line == TERMINATOR => terminated = true,
            _ => tracing::debug!(line, "ignoring unrecognised line from spawn.sh"),
        }
    }
    if let Some(msg) = error {
        return Err(NvmuxError::Session(crate::error::SessionError::NotFound(
            msg,
        )));
    }
    if !terminated {
        return Err(NvmuxError::Session(crate::error::SessionError::NotFound(
            format!("the spawn command did not complete (no {TERMINATOR} marker)"),
        )));
    }
    Ok(out)
}

/// What `probe.sh` reported about a session host.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostProbe {
    /// The runtime directory on the host that owns the sessions.
    pub runtime_dir: String,
    /// The first line of `nvim --version`, empty if nvim is not on PATH there.
    pub nvim_banner: String,
}

/// Parse the output of `probe.sh`.
pub fn parse_probe(stdout: &str) -> Result<HostProbe> {
    let mut out = HostProbe::default();
    let mut terminated = false;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line == TERMINATOR {
            terminated = true;
        } else if let Some(v) = line.strip_prefix("DIR ") {
            out.runtime_dir = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("NVIM ") {
            out.nvim_banner = v.trim().to_string();
        }
    }
    if !terminated || out.runtime_dir.is_empty() {
        return Err(NvmuxError::Session(crate::error::SessionError::NotFound(
            "could not read the remote runtime directory".to_string(),
        )));
    }
    Ok(out)
}

/// What `kill.sh` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// The process is gone and its files were removed.
    Killed,
    /// There was no such process; the files were removed.
    Absent,
    /// It was signalled and is still alive; nothing was removed.
    Orphaned,
    /// We could not determine what owns the socket, so nothing was signalled
    /// and nothing was removed.
    Unknown,
}

/// Parse the output of `kill.sh`.
pub fn parse_kill(stdout: &str) -> Result<KillOutcome> {
    let mut outcome = None;
    let mut terminated = false;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line == TERMINATOR {
            terminated = true;
            continue;
        }
        if let Some(v) = line.strip_prefix("RESULT ") {
            outcome = match v.trim() {
                "killed" => Some(KillOutcome::Killed),
                "absent" => Some(KillOutcome::Absent),
                "orphaned" => Some(KillOutcome::Orphaned),
                "unknown" => Some(KillOutcome::Unknown),
                _ => None,
            };
        }
    }
    if !terminated {
        return Err(NvmuxError::Session(crate::error::SessionError::NotFound(
            format!("the kill command did not complete (no {TERMINATOR} marker)"),
        )));
    }
    outcome.ok_or_else(|| {
        NvmuxError::Session(crate::error::SessionError::NotFound(
            "the kill command reported no outcome".to_string(),
        ))
    })
}

/// Turn parsed rows into sessions, dropping ones whose metadata is unusable.
///
/// A socket with no readable metadata is reported rather than silently hidden:
/// it means an interrupted create, and the user should be able to see and kill
/// it. It gets a placeholder name derived from its id.
pub fn rows_to_sessions(rows: Vec<Listed>) -> Vec<Session> {
    rows.into_iter()
        .filter(|row| {
            // A malformed id means something not ours is in the runtime
            // directory. Skip it rather than turning it into a path.
            let ok = crate::ids::is_valid_id(&row.id);
            if !ok {
                tracing::warn!(id = %row.id, "ignoring a file with a malformed session id");
            }
            ok
        })
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

            // The FILENAME is the identity, not the `id` field inside the file.
            // Every path nvmux builds derives from it, so trusting the contents
            // would let a hand-edited `<id>.json` aim a kill somewhere else.
            if session.id != row.id {
                tracing::warn!(
                    file_id = %session.id,
                    filename_id = %row.id,
                    "session metadata claims a different id; using the filename"
                );
                session.id = row.id.clone();
            }
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
        // `splitn(4)` keeps tabs inside the json field rather than shifting
        // later fields. list.sh strips them too.
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

    #[test]
    fn parses_a_successful_spawn() {
        let s = parse_spawn("PID 4242\nSOCK ok\nNVMUX_END\n").expect("parse");
        assert_eq!(s.pid, Some(4242));
        assert!(s.socket_appeared);
    }

    #[test]
    fn an_unvalidated_pid_is_none_not_zero() {
        // An empty PID line must not become 0, or a pid we then signal.
        let s = parse_spawn("PID \nSOCK ok\nNVMUX_END\n").expect("parse");
        assert_eq!(s.pid, None, "an unconfirmed pid must not become a number");

        for dangerous in ["PID 0\n", "PID 1\n", "PID nonsense\n"] {
            let s = parse_spawn(&format!("{dangerous}SOCK ok\nNVMUX_END\n")).expect("parse");
            assert_eq!(
                s.pid, None,
                "{dangerous:?} must not yield a signallable pid"
            );
        }
    }

    #[test]
    fn a_timed_out_socket_is_reported() {
        let s = parse_spawn("PID 7\nSOCK timeout\nNVMUX_END\n").expect("parse");
        assert!(!s.socket_appeared);
    }

    #[test]
    fn spawn_output_without_a_terminator_is_an_error() {
        assert!(parse_spawn("PID 4242\nSOCK ok\n").is_err());
        assert!(parse_spawn("").is_err());
    }

    #[test]
    fn shell_noise_around_spawn_output_is_ignored() {
        let s = parse_spawn("Welcome to Ubuntu\nPID 9\nSOCK ok\nNVMUX_END\n").expect("parse");
        assert_eq!(s.pid, Some(9));
    }

    #[test]
    fn parses_every_kill_outcome() {
        for (text, want) in [
            ("killed", KillOutcome::Killed),
            ("absent", KillOutcome::Absent),
            ("orphaned", KillOutcome::Orphaned),
            ("unknown", KillOutcome::Unknown),
        ] {
            let out = format!("RESULT {text}\nNVMUX_END\n");
            assert_eq!(parse_kill(&out).expect("parse"), want);
        }
    }

    #[test]
    fn kill_output_without_an_outcome_is_an_error() {
        assert!(parse_kill("NVMUX_END\n").is_err(), "no RESULT line");
        assert!(parse_kill("RESULT killed\n").is_err(), "no terminator");
        assert!(parse_kill("RESULT nonsense\nNVMUX_END\n").is_err());
        assert!(parse_kill("").is_err());
    }

    /// The filename is the identity; a metadata file claiming another id must
    /// not be able to redirect a kill or a rename at a different session.
    #[test]
    fn a_metadata_file_cannot_claim_another_sessions_id() {
        let out = format!(
            "S\tabcdefgh\t1\t{}\nNVMUX_END\n",
            r#"{"id":"zzzzzzzz","name":"liar","created":1,"pid":2}"#
        );
        let sessions = rows_to_sessions(parse_listing(&out).expect("parse"));
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].id, "abcdefgh",
            "the id must come from the filename, not the file body"
        );
    }

    /// An id that would escape the runtime directory must never become a path.
    #[test]
    fn malformed_ids_are_dropped_from_the_listing() {
        for bad in [
            "../../etc/passwd",
            "..",
            "sh0rt",
            "way-too-long-to-be-an-id",
        ] {
            let out = format!("S\t{bad}\t1\t\nNVMUX_END\n");
            let sessions = rows_to_sessions(parse_listing(&out).expect("parse"));
            assert!(sessions.is_empty(), "{bad:?} should have been dropped");
        }
    }

    #[test]
    fn parses_a_host_probe() {
        let p = parse_probe("DIR /tmp/nvmux-501\nNVIM NVIM v0.11.4\nNVMUX_END\n").expect("parse");
        assert_eq!(p.runtime_dir, "/tmp/nvmux-501");
        assert_eq!(p.nvim_banner, "NVIM v0.11.4");
    }

    #[test]
    fn a_host_without_nvim_probes_with_an_empty_banner() {
        let p = parse_probe("DIR /tmp/nvmux-0\nNVIM\nNVMUX_END\n").expect("parse");
        assert_eq!(p.runtime_dir, "/tmp/nvmux-0");
        assert!(p.nvim_banner.is_empty());
    }

    #[test]
    fn a_login_shell_banner_does_not_confuse_the_probe() {
        let out = "Welcome to Ubuntu 24.04\n\
                   Last login: Thu\n\
                   DIR /tmp/nvmux-1000\n\
                   NVIM NVIM v0.11.4\n\
                   NVMUX_END\n";
        let p = parse_probe(out).expect("parse");
        assert_eq!(p.runtime_dir, "/tmp/nvmux-1000");
    }

    #[test]
    fn an_incomplete_probe_is_an_error() {
        assert!(parse_probe("DIR /tmp/x\n").is_err(), "no terminator");
        assert!(parse_probe("NVMUX_END\n").is_err(), "no directory");
        assert!(parse_probe("").is_err());
    }
}
