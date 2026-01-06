mod provision;

use clap::Parser;
use crate::provision::{Cli, Commands, provision};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(home_dir) = home::home_dir() {
        let config_path = home_dir.join(".config/roa/roa_variables");

        dotenvy::from_path(config_path).ok();
    }
    
    let cli = Cli::parse();

    match cli.command {
        Commands::Provision(args) => {
            provision(args).await?;
        }
    }

    Ok(())
}
