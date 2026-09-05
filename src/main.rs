//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{config, logging, nvim, pty, transport, ui};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Everything else depends on this existing and being ours.
    let dir = config::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // Before anything is spawned: see `config::restrict_umask`.
    config::restrict_umask();

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

    let transport = transport::open(location.clone())?;
    session_loop(transport.as_ref())
}

/// Alternate between the picker and an attached session until the user leaves.
///
/// The attachment is carried across iterations, so `Ctrl-t t`, `Ctrl-t c` and
/// `Ctrl-t ?` come back to the *same* client rather than starting a new one.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    let mut message: Option<String> = None;

    loop {
        // `Ctrl-t c` moves this to the session it just created.
        let mut current = match ui::run(transport, message.take())? {
            ui::Outcome::Quit => break,
            ui::Outcome::Attach(session) => session,
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
                    message = Some(describe_attach_failure(transport, &e));
                    break;
                }
            };

            match pty::relay(attachment)? {
                (pty::Outcome::ToPicker, held) => {
                    attached = held;
                    break;
                }
                (pty::Outcome::Detached, _) => return Ok(()),
                (pty::Outcome::ChildExited, _) => break,
                (pty::Outcome::CreateNew, held) => {
                    // Held rather than killed, so a cancelled prompt resumes it.
                    attached = held;
                    if let ui::prompt::Outcome::Created(session) = ui::prompt::run(transport)? {
                        current = session;
                    }
                    continue;
                }
                (pty::Outcome::ShowHelp, held) => {
                    attached = held;
                    ui::help::run()?;
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
fn describe_attach_failure(transport: &dyn transport::Transport, e: &nvmux::NvmuxError) -> String {
    let remote = matches!(transport.location(), transport::Location::Ssh(_));
    let host = transport.location().to_string();
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
