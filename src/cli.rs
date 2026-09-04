//! The command line.
//!
//! Exactly two invocations, and no subcommands:
//!
//! ```text
//! nvmux           manage sessions on this machine
//! nvmux <host>    manage sessions on <host>
//! ```
//!
//! plus `--version` and `--help`. That is the whole surface for v0.1.

use clap::Parser;

use crate::transport::Location;

#[derive(Debug, Parser)]
#[command(
    name = "nvmux",
    // clap takes the usage line from argv[0], not from `name`, so a binary
    // invoked through a symlink would otherwise print someone else's name.
    bin_name = "nvmux",
    version,
    about = "A tmux-style session manager for Neovim",
    long_about = "A tmux-style session manager for Neovim.\n\n\
                  Run with no arguments to manage sessions on this machine, or \
                  pass a host to manage sessions there. Sessions keep running \
                  after you detach; the editor runs on the session host and all \
                  rendering happens locally.",
    after_help = "Press Ctrl-t d to detach from a session. Avoid :q -- in a \
                  remote UI it quits the server, not just your view."
)]
pub struct Cli {
    /// Host to manage sessions on; passed to ssh verbatim.
    ///
    /// Accepts anything ssh does: a hostname, `user@host`, or an alias from
    /// your `~/.ssh/config`.
    #[arg(value_name = "HOST")]
    pub host: Option<String>,
}

impl Cli {
    pub fn location(&self) -> Location {
        match &self.host {
            Some(h) => Location::Ssh(h.clone()),
            None => Location::Local,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn bare_invocation_is_local() {
        let cli = Cli::try_parse_from(["nvmux"]).expect("parse");
        assert_eq!(cli.location(), Location::Local);
    }

    #[test]
    fn a_host_argument_selects_ssh() {
        for host in ["myhost", "user@myhost", "my-ssh-config-alias", "1.2.3.4"] {
            let cli = Cli::try_parse_from(["nvmux", host]).expect("parse");
            assert_eq!(
                cli.location(),
                Location::Ssh(host.to_string()),
                "host must be passed through verbatim"
            );
        }
    }

    #[test]
    fn there_are_no_subcommands() {
        // The surface is deliberately two invocations. If a subcommand is ever
        // added, this test is the reminder that it was a decision.
        assert!(
            Cli::command().get_subcommands().next().is_none(),
            "v0.1 has no subcommands"
        );
    }

    #[test]
    fn a_second_positional_is_rejected() {
        assert!(Cli::try_parse_from(["nvmux", "a", "b"]).is_err());
    }

    #[test]
    fn version_and_help_exist() {
        let err = Cli::try_parse_from(["nvmux", "--version"]).expect_err("exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        let err = Cli::try_parse_from(["nvmux", "--help"]).expect_err("exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
    }
}
