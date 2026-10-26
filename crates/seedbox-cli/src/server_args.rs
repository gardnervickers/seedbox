//! Server location shared by explicit and automatic server startup.

use clap::Args;
use std::path::PathBuf;

#[derive(Args, Clone, Debug, Default)]
#[command(next_help_heading = "Server options")]
pub(super) struct ServerArgs {
    /// Server state directory [default: user state directory].
    #[arg(long, env = "SEEDBOX_STATE", global = true)]
    pub state: Option<PathBuf>,
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
    fn server_location_is_shared_by_explicit_and_automatic_startup() {
        for words in [
            vec!["seedbox", "--state", "/tmp/seedbox", "run"],
            vec!["seedbox", "run", "--state", "/tmp/seedbox"],
            vec!["seedbox", "server", "start", "--state", "/tmp/seedbox"],
        ] {
            let parsed = TestCli::try_parse_from(words).unwrap();
            assert_eq!(
                parsed.server.state.as_deref(),
                Some(std::path::Path::new("/tmp/seedbox"))
            );
        }
    }
}
