mod provision;

use clap::Parser;
use crate::provision::{Cli, Commands, provision};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Provision(args) => {
            provision(args).await?;
        }
    }

    Ok(())
}
