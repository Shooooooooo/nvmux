//! Binary entry point.

use anyhow::{Context, Result};
use clap::Parser;

use nvmux::cli::Cli;
use nvmux::transport::Location;
use nvmux::{config, logging, nvim, transport};

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
    let sessions = transport.list_sessions()?;

    // Milestone 1 has no picker yet, so the bare invocation prints what it
    // found. Milestone 3 replaces this with the ratatui UI; the CLI surface
    // does not change.
    println!("nvmux {} — {}", env!("CARGO_PKG_VERSION"), location);
    println!("neovim {local_nvim}");
    println!("runtime dir {}", config::runtime_dir().display());
    println!();
    if sessions.is_empty() {
        println!("no sessions");
    } else {
        for s in &sessions {
            println!("  {:<10}  {:<24}  {:?}", s.id, s.name, s.state.liveness);
        }
    }

    if matches!(location, Location::Ssh(_)) {
        println!("\n(ssh transport arrives in milestone 5)");
    }
    Ok(())
}
