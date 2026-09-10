//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{config, logging, nested, nvim, paths, pty, transport, ui};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Everything else depends on this existing and being ours.
    let dir = paths::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // Before anything is spawned — and before a first-run config file is written
    // in `run` — restrict the umask: see `paths::restrict_umask`.
    paths::restrict_umask();

    // Before any screen is drawn, so a `kill` during the picker — not only
    // during an attached session — hands back a usable terminal.
    nvmux::term::install_signal_safety_net();

    if let Err(e) = run(&cli) {
        // A plain message, no backtrace: every error here is meant to be
        // actionable on its own.
        eprintln!("nvmux: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    // Before the version check, before the config, and before any first-run
    // prompt: a second nvmux inside a session cannot work, and the less it has
    // done by the time it says so the better.
    nested::check()?;

    let location = cli.location();

    // Checked up front rather than surfacing later as an unexplained connection
    // failure. This is the nvim used as the --remote-ui client; the remote one
    // is checked by the SSH transport when it connects.
    let local_nvim = nvim::check_local()?;
    tracing::debug!(version = %local_nvim, "local nvim");

    // Config is a local concern — the prefix machine and the picker both run
    // here — so it is established before any transport, `nvmux <host>`
    // included. On a genuine first run at an interactive terminal this asks for a
    // prefix and records it; otherwise it loads whatever exists (or the defaults).
    config::init(establish_settings()?);

    let transport = transport::open(location.clone())?;
    session_loop(transport.as_ref())
}

/// Decide this run's settings, prompting once on a true first run.
///
/// A first run is: no config file at the default path, `$NVMUX_CONFIG` unset, and
/// an interactive terminal (both stdin and stdout). Anything else — a file
/// already there, an explicit config, a piped/non-interactive run — just loads
/// normally. Failing to *write* the chosen config is reported but not fatal: the
/// prefix still applies this session, and the next run will ask again.
fn establish_settings() -> Result<config::Settings> {
    use std::io::IsTerminal;

    match config::first_run_target() {
        Some(path) if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => {
            match ui::setup::run()? {
                ui::setup::Outcome::Chosen(prefix) => {
                    if let Err(e) = config::write_default(&path, prefix) {
                        eprintln!("nvmux: could not write {}: {e}", path.display());
                    }
                    Ok(config::with_prefix(prefix))
                }
                ui::setup::Outcome::Skipped => Ok(config::Settings::default()),
            }
        }
        _ => Ok(config::load()?),
    }
}

/// Alternate between the picker and an attached session until the user leaves.
///
/// The attachment is carried across iterations, so `<prefix> Space`, `<prefix> c` and
/// `<prefix> ?` come back to the *same* client rather than starting a new one.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    let mut message: Option<String> = None;
    // The session the last trip through the picker led to, so the next one
    // opens with the cursor on it rather than on the first row.
    let mut focus: Option<String> = None;

    loop {
        // `<prefix> c` moves this to the session it just created.
        let (mut current, mut highest) = match ui::run(transport, message.take(), focus.as_deref())?
        {
            ui::Outcome::Quit => {
                // A client held across `<prefix> Space` is retired explicitly; its
                // `Drop` would do the same, this just says so.
                if let Some(a) = attached.take() {
                    a.terminate();
                }
                break;
            }
            ui::Outcome::Attach { session, highest } => (session, highest),
        };

        loop {
            let opened = match attached.take() {
                Some(a) if a.session_id == current.id => Ok(a),
                // Retiring the old client leaves its server running: killing a
                // --remote-ui client does not kill a --headless --listen server.
                //
                // Hung up first and reaped after, with the new client's spawn in
                // between: the two have nothing to say to each other — a
                // different session, a different server, a different pty — so
                // serialising them would charge the user the sum of the two.
                // Nothing reads the old pty master again either, so the dying
                // client's own restore sequence goes nowhere near the terminal.
                Some(mut other) => {
                    other.hang_up();
                    let spawned = new_attachment(transport, &current);
                    // Explicitly, rather than by falling out of the arm: this is
                    // the wait the hangup deferred, and leaving it to a binding's
                    // drop would let the next edit here re-serialise it without
                    // noticing. It has to happen on the failure path too, which
                    // is why the spawn is held rather than returned from inside.
                    drop(other);
                    spawned
                }
                None => new_attachment(transport, &current),
            };

            // A failed attach must not end the program: the user can only act
            // on it from the picker, with the reason on screen.
            let attachment = match opened {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(id = %current.id, error = %e, "attach failed");
                    message = Some(describe_attach_failure(transport.location(), &e));
                    break;
                }
            };

            match pty::relay(attachment, highest)? {
                (pty::Outcome::ToPicker, held) => {
                    attached = held;
                    break;
                }
                // The session keeps running either way: on `<prefix> d`
                // because the user asked, on a closed stdin because there is
                // no terminal left to ask from.
                (pty::Outcome::Detached | pty::Outcome::StdinClosed, _) => return Ok(()),
                (pty::Outcome::ChildExited, _) => break,
                (pty::Outcome::CreateNew, held) => {
                    // Held rather than killed, so a cancelled prompt resumes it.
                    attached = held;
                    if let ui::prompt::Outcome::Created(session) = ui::prompt::run(transport)? {
                        // A new session can be numbered above anything the
                        // relay was told about, and the hint decides how long a
                        // digit waits.
                        highest = highest.max(session.state.num);
                        current = session;
                    }
                    continue;
                }
                (pty::Outcome::ShowHelp, held) => {
                    attached = held;
                    ui::help::run()?;
                    continue;
                }
                (pty::Outcome::Switch(num), held) => {
                    // Held rather than killed, so a number that names nothing
                    // puts the user straight back where they were.
                    attached = held;
                    // A failed listing goes back to the picker like a failed
                    // attach, and for a second reason besides: this is the one
                    // path from one relay straight into another, so the screen
                    // an error would land on is the cleared one the held branch
                    // of `pty::relay` just handed over — nothing else on it, and
                    // nothing the user could do from it. The hint row is both,
                    // next to the row the message is about.
                    let sessions = match transport.list_sessions() {
                        Ok(sessions) => sessions,
                        Err(e) => {
                            message = Some(describe_listing_failure(&e));
                            break;
                        }
                    };
                    highest = ui::highest_num(&sessions);
                    match sessions.into_iter().find(|s| s.state.num == num) {
                        // The loop above retires the old client and attaches the
                        // new one; an unchanged id reuses the client as it is.
                        Some(session) => current = session,
                        None => tracing::debug!(num, "no session with that number"),
                    }
                    continue;
                }
            }
        }

        // Every way out of the relay loop leads back to the picker, and every
        // one of them was showing `current` — including the failures, where the
        // cursor lands on the row the message on the hint line is about.
        focus = Some(current.id.clone());
    }
    Ok(())
}

/// Explain why an attach failed, in terms of what actually went wrong.
///
/// Through an SSH forward, ECONNREFUSED means the *ControlMaster* died, not the
/// session: ssh accepts first and resets afterwards when it is the remote
/// process that has gone. Verified both ways — a dead remote nvim gives
/// ECONNRESET, a dead master gives ECONNREFUSED.
fn describe_attach_failure(location: &transport::Location, e: &nvmux::NvmuxError) -> String {
    let remote = matches!(location, transport::Location::Ssh(_));
    let host = location.to_string();
    match e {
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::ConnectionRefused(_)) if remote => {
            format!("the connection to {host} dropped — press enter to retry")
        }
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Reset) if remote => {
            format!("that session is no longer running on {host}")
        }
        nvmux::NvmuxError::Rpc(rpc) if rpc.is_definitely_dead() => "that session is gone".into(),
        // Reachable, and not answering: a `:!make` still running, a prompt
        // nvmux will not answer for the user, CPU-bound Lua. The bare error
        // ("timed out after 3s") reads as if nvmux had lost the session.
        nvmux::NvmuxError::Rpc(nvmux::error::RpcError::Timeout(after)) => {
            format!("that session is busy and did not answer within {after:?}")
        }
        other => one_line(other),
    }
}

/// Explain why a switch could not find out what to switch to.
///
/// Unlike an attach this is a listing, and the errors it raises already name the
/// host where they have one (`ssh: the connection to myhost died`). What none of
/// them says is what nvmux was attempting — a script's refusal is rendered bare,
/// on purpose — and on the hint row there is nothing else to say it.
fn describe_listing_failure(e: &nvmux::NvmuxError) -> String {
    format!("could not list sessions: {}", one_line(e))
}

/// Collapse an error to something that fits on one line.
///
/// The hint row it lands on is exactly one row; a multi-line error would be
/// truncated at the first newline and lose the part that explains itself.
fn one_line(e: &nvmux::NvmuxError) -> String {
    e.to_string().lines().collect::<Vec<_>>().join(" — ")
}

/// Returns the crate's own error type rather than `anyhow`, so the caller can
/// tell a dropped connection from a dead session and say the right thing.
fn new_attachment(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
) -> nvmux::Result<pty::Attachment> {
    let sock = transport.local_socket_for(session)?;
    pty::spawn(&session.id, &sock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvmux::error::{RpcError, SessionError, SshError};
    use nvmux::NvmuxError;
    use transport::Location;

    fn refused() -> NvmuxError {
        NvmuxError::Rpc(RpcError::ConnectionRefused("x.sock".into()))
    }

    /// Through a forward, "refused" is the master, not the session.
    #[test]
    fn a_refused_forward_blames_the_connection_not_the_session() {
        let msg = describe_attach_failure(&Location::Ssh("myhost".into()), &refused());
        assert!(msg.contains("myhost"), "{msg}");
        assert!(msg.contains("dropped"), "{msg}");
        assert!(msg.contains("retry"), "{msg}");
    }

    #[test]
    fn a_reset_forward_means_the_remote_session_is_gone() {
        let msg = describe_attach_failure(
            &Location::Ssh("myhost".into()),
            &NvmuxError::Rpc(RpcError::Reset),
        );
        assert!(msg.contains("no longer running on myhost"), "{msg}");
    }

    /// Locally the same errno means the session itself.
    #[test]
    fn a_refused_local_socket_means_the_session_is_gone() {
        let msg = describe_attach_failure(&Location::Local, &refused());
        assert_eq!(msg, "that session is gone");
    }

    /// The hint row is one row: any other error is flattened onto it.
    #[test]
    fn other_errors_are_flattened_to_one_line() {
        let e = NvmuxError::Session(SessionError::NotReady {
            name: "x".into(),
            timeout: std::time::Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "line one\nline two".into(),
        });
        let msg = describe_attach_failure(&Location::Local, &e);
        assert!(!msg.contains('\n'), "{msg:?}");
        assert!(
            msg.contains("line one") && msg.contains("line two"),
            "{msg:?}"
        );
    }

    /// A listing failure reaches the picker with no other context around it, so
    /// the message has to say what was being attempted as well as what failed.
    #[test]
    fn a_failed_listing_says_what_nvmux_was_doing() {
        let e = NvmuxError::Ssh(SshError::MasterDied("myhost".into()));
        let msg = describe_listing_failure(&e);
        assert!(msg.contains("list sessions"), "{msg}");
        assert!(msg.contains("myhost"), "{msg}");
    }

    /// A script's refusal is rendered bare on purpose, so on its own it reads as
    /// a statement about nothing in particular. The prefix is what anchors it.
    #[test]
    fn a_bare_script_failure_is_not_left_to_speak_for_itself() {
        let e = NvmuxError::Session(SessionError::ScriptFailed(
            "runtime directory /tmp/nvmux-1000 is not owned by us".into(),
        ));
        let msg = describe_listing_failure(&e);
        assert!(msg.contains("could not list sessions"), "{msg}");
        assert!(msg.contains("is not owned by us"), "{msg}");
    }

    /// The hint row is one row here too.
    #[test]
    fn a_multi_line_listing_failure_is_flattened_to_one_line() {
        let e = NvmuxError::Session(SessionError::NotReady {
            name: "x".into(),
            timeout: std::time::Duration::from_secs(1),
            log: "x.log".into(),
            log_tail: "line one\nline two".into(),
        });
        let msg = describe_listing_failure(&e);
        assert!(!msg.contains('\n'), "{msg:?}");
        assert!(
            msg.contains("line one") && msg.contains("line two"),
            "{msg:?}"
        );
    }
}
