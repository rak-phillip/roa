use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ec2::Client;
use clap::{Parser};

#[derive(Parser, Debug)]
pub struct TerminateArgs {
    #[clap(long)]
    instance_id: String,
}

pub async fn terminate(args: TerminateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-west-2");
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);

    let resp = client.terminate_instances()
        .instance_ids(args.instance_id)
        .send()
        .await?;

    let terminating_instance = resp.terminating_instances()
        .first()
        .and_then(|instance| instance.instance_id())
        .unwrap_or("<unknown>");

    println!("Terminating instance: {}", terminating_instance);

    Ok(())
}
