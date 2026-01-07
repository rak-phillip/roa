mod provision;
mod terminate;

use clap::{Parser, Subcommand};
use crate::provision::{provision, ProvisionArgs};
use crate::terminate::{terminate, TerminateArgs};

#[derive(Parser, Debug)]
#[command(name = "roa", about = "Rancher on AWS", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands{
    Provision(ProvisionArgs),
    Terminate(TerminateArgs),
}

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
        },
        Commands::Terminate(args) => {
            terminate(args).await?;
        }
    }

    Ok(())
}
