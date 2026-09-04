//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::{config, logging, nvim, transport, ui};

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

    match ui::run(transport.as_ref())? {
        ui::Outcome::Quit => Ok(()),
        ui::Outcome::Attach(session) => {
            // Milestone 4 replaces this with the PTY proxy. Until then, say what
            // would have happened rather than pretending to attach.
            let sock = transport.local_socket_for(&session)?;
            println!("would attach to {:?} ({})", session.name, sock.display());
            println!("(the PTY proxy arrives in milestone 4)");
            Ok(())
        }
    }
}
