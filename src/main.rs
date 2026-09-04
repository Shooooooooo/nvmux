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
/// returning to the picker feel free.
fn session_loop(transport: &dyn transport::Transport) -> Result<()> {
    let mut attached: Option<pty::Attachment> = None;

    loop {
        let chosen = match ui::run(transport)? {
            ui::Outcome::Quit => break,
            ui::Outcome::Attach(session) => session,
        };

        loop {
            let attachment = match attached.take() {
                // Same session: resume the client that is already running.
                Some(a) if a.session_id == chosen.id => a,
                // A different session was attached; that client is finished
                // with. Its server keeps running — killing a --remote-ui client
                // does not kill a --headless --listen server.
                Some(other) => {
                    other.terminate();
                    new_attachment(transport, &chosen)?
                }
                None => new_attachment(transport, &chosen)?,
            };

            match pty::relay(attachment)? {
                (pty::Outcome::ToPicker, held) => {
                    attached = held;
                    break;
                }
                (pty::Outcome::Detached, _) => return Ok(()),
                (pty::Outcome::ChildExited, _) => break,
                (pty::Outcome::CreateNew, _) => {
                    // Ctrl-t c: straight into a new session without a detour
                    // through the picker.
                    match transport.create_session(&next_name(transport)?) {
                        Ok(session) => {
                            attached = Some(new_attachment(transport, &session)?);
                            continue;
                        }
                        Err(e) => {
                            eprintln!("nvmux: {e}");
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn new_attachment(
    transport: &dyn transport::Transport,
    session: &nvmux::session::Session,
) -> Result<pty::Attachment> {
    let sock = transport.local_socket_for(session)?;
    Ok(pty::spawn(&session.id, &sock)?)
}

/// A name for a session created with `Ctrl-t c`, which has no prompt to type in.
fn next_name(transport: &dyn transport::Transport) -> Result<String> {
    let taken: Vec<String> = transport
        .list_sessions()?
        .into_iter()
        .map(|s| s.name.to_lowercase())
        .collect();
    for n in 1..1000 {
        let candidate = format!("session {n}");
        if !taken.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Ok("session".to_string())
}
