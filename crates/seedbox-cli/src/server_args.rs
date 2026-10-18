//! Shared configuration for explicit and automatic server startup.

use clap::Args;
use std::path::PathBuf;

#[derive(Args, Clone, Debug, Default)]
#[command(next_help_heading = "Server options")]
pub(super) struct ServerArgs {
    /// Server state directory [default: user state directory].
    #[arg(long, env = "SEEDBOX_STATE", global = true)]
    pub state: Option<PathBuf>,
    /// Record full machine state when starting a server.
    #[arg(long, global = true)]
    pub full_state: bool,
}

impl ServerArgs {
    pub(super) fn inputs(&self) -> super::RuntimeInputs {
        super::RuntimeInputs {
            full_state: self.full_state,
        }
    }

    pub(super) fn append_startup_args(&self, command: &mut std::process::Command) {
        if self.full_state {
            command.arg("--full-state");
        }
    }

    pub(super) fn existing_server_notice(&self) {
        if self.full_state {
            eprintln!("server already running; --full-state applies only when starting a server; restart it with `seedbox server start --full-state` to change its configuration");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Parser, Subcommand};

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        server: ServerArgs,
        #[command(subcommand)]
        command: TestCommand,
    }

    #[derive(Subcommand)]
    enum TestCommand {
        Run,
        Server {
            #[command(subcommand)]
            command: ServerCommand,
        },
    }

    #[derive(Subcommand)]
    enum ServerCommand {
        Start,
    }

    #[test]
    fn server_configuration_is_shared_by_explicit_and_automatic_startup() {
        for words in [
            vec!["seedbox", "--full-state", "--state", "/tmp/seedbox", "run"],
            vec!["seedbox", "run", "--full-state", "--state", "/tmp/seedbox"],
            vec![
                "seedbox",
                "server",
                "start",
                "--full-state",
                "--state",
                "/tmp/seedbox",
            ],
        ] {
            let parsed = TestCli::try_parse_from(words).unwrap();
            assert!(parsed.server.full_state);
            assert_eq!(
                parsed.server.state.as_deref(),
                Some(std::path::Path::new("/tmp/seedbox"))
            );
            let mut child = std::process::Command::new("seedbox");
            parsed.server.append_startup_args(&mut child);
            assert_eq!(child.get_args().collect::<Vec<_>>(), ["--full-state"]);
        }
    }
}
