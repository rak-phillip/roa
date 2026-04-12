mod provision;
mod terminate;
mod network;
mod list;
mod instance;

use clap::{Parser, Subcommand};
use crate::list::{list, ListArgs};
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
    #[clap(about = "Launches an EC2 instance, creates a security group, and registers a DNS A record")]
    Provision(ProvisionArgs),
    #[clap(about = "Terminates the EC2 instance and cleans up the Route 53 DNS record and the security group")]
    Terminate(TerminateArgs),
    #[clap(about = "Displays all instances recorded in the local manifest")]
    List(ListArgs),
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
        },
        Commands::List(args) => {
            list(args).await?;
        }
    }

    Ok(())
}
