//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{config, logging, nvim, paths, pty, transport, ui};

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
    let location = cli.location();

    // Checked up front rather than surfacing later as an unexplained connection
    // failure. This is the nvim used as the --remote-ui client; the remote one
    // is checked by the SSH transport when it connects.
    let local_nvim = nvim::check_local()?;
    tracing::debug!(version = %local_nvim, "local nvim");

    // Config is a local concern — the prefix machine, the fade and the picker all
    // run here — so it is established before any transport, `nvmux <host>`
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
/// The attachment is carried across iterations, so `<prefix> t`, `<prefix> c` and
/// `<prefix> ?` come back to the *same* client rather than starting a new one.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    let mut message: Option<String> = None;

    loop {
        // `<prefix> c` moves this to the session it just created.
        let (mut current, mut highest) = match ui::run(transport, message.take())? {
            ui::Outcome::Quit => {
                // A client held across `<prefix> t` is retired explicitly; its
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
                Some(other) => {
                    other.terminate();
                    new_attachment(transport, &current)
                }
                None => new_attachment(transport, &current),
            };

            // A failed attach must not end the program: the user can only act
            // on it from the picker, with the reason on screen.
            let attachment = match opened {
                Ok(a) => a,
                Err(e) => {
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
                    let sessions = transport.list_sessions()?;
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
        other => other.to_string().lines().collect::<Vec<_>>().join(" — "),
    }
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
    use nvmux::error::RpcError;
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
        let e = NvmuxError::Session(nvmux::error::SessionError::NotReady {
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
}
