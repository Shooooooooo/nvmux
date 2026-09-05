//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{config, logging, nvim, pty, transport, ui};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Everything else depends on this existing and being ours, so it happens
    // first and its failure is reported plainly.
    let dir = config::ensure_runtime_dir().context("preparing the nvmux runtime directory")?;
    logging::init(&dir)?;

    // Neovim creates its listen socket with 0777 & ~umask, so a permissive mask
    // would leave a socket any local user could drive. Set this before anything
    // is spawned.
    config::restrict_umask();

    if let Err(e) = run(&cli) {
        // Errors go to stderr as a plain message. No backtrace spew: every error
        // this program produces is meant to be actionable on its own.
        eprintln!("nvmux: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(cli: &Cli) -> Result<()> {
    let location = cli.location();

    // Checked up front rather than surfacing later as an unexplained connection
    // failure. The local nvim is the one used as the --remote-ui client; the
    // remote one is checked by the SSH transport when it connects (milestone 5).
    let local_nvim = nvim::check_local()?;
    tracing::debug!(version = %local_nvim, "local nvim");

    let transport = transport::open(location.clone())?;
    session_loop(transport.as_ref())
}

/// Alternate between the picker and an attached session until the user leaves.
///
/// The attachment is carried across iterations so that `Ctrl-t t` can come back
/// to the *same* client rather than starting a new one, which is what makes
/// returning to the picker feel free. `Ctrl-t c` rides on the same machinery:
/// the client survives the prompt, so cancelling it costs nothing either.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;
    let mut message: Option<String> = None;

    loop {
        // The session the inner loop is about. `Ctrl-t c` moves it to the
        // session it just created; everything else leaves it alone.
        let mut current = match ui::run(transport, message.take())? {
            ui::Outcome::Quit => break,
            ui::Outcome::Attach(session) => session,
        };

        loop {
            let opened = match attached.take() {
                // Same session: resume the client that is already running.
                Some(a) if a.session_id == current.id => Ok(a),
                // A different session was attached; that client is finished
                // with. Its server keeps running — killing a --remote-ui client
                // does not kill a --headless --listen server.
                Some(other) => {
                    other.terminate();
                    new_attachment(transport, &current)
                }
                None => new_attachment(transport, &current),
            };

            // A failed attach must not end the program. A dropped connection,
            // or a session that died while the picker was open, is something
            // the user can act on — but only if they are still in the picker to
            // do it, with the reason on screen.
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
                    // Ctrl-t c: name a new session and go straight to it,
                    // without a detour through the picker. The old client is
                    // held onto rather than killed, so a cancelled prompt just
                    // resumes it; naming one instead moves `current`, and the
                    // match above then retires the old client for us.
                    attached = held;
                    if let ui::prompt::Outcome::Created(session) = ui::prompt::run(transport)? {
                        current = session;
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
/// "connection refused" is accurate for a local socket and misleading for a
/// remote one: through an SSH forward that errno means the *ControlMaster*
/// died, not the session — ssh accepts first and resets afterwards when the
/// remote process is the one that has gone. Verified both ways: a dead remote
/// nvim gives ECONNRESET, a dead master gives ECONNREFUSED.
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
