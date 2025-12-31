use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "roa", about = "Rancher on AWS", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands{
    Provision(ProvisionArgs),
}

#[derive(Parser, Debug)]
struct ProvisionArgs {
    #[arg(long = "name")]
    name: String,

    #[arg(long, default_value_t = 64)]
    storage_gb: i32,

    #[arg(long)]
    vpc_id: String,

    #[arg(long)]
    subnet_id: String,

    #[arg(long)]
    key_name: String,
}

