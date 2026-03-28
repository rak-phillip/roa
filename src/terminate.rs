use std::error::Error;
use std::time::Duration;
use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ec2::{Client as Ec2Client, Client};
use aws_sdk_route53::Client as Route53Client;
use aws_sdk_ec2::types::Filter;
use aws_sdk_route53::types::{ResourceRecord, ResourceRecordSet, RrType, Change, ChangeAction, ChangeBatch};
use clap::{Parser};
use tokio::time::sleep;

#[derive(Parser, Debug)]
pub struct TerminateArgs {
    #[clap(long)]
    instance_id: String,

    #[arg(long, env = "ROA_HOSTED_ZONE_ID")]
    hosted_zone_id: String,

    #[arg(long, env = "ROA_VPC_ID")]
    vpc_id: String,
}

pub async fn terminate(args: TerminateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-west-2");
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;

    let ec2 = Ec2Client::new(&config);
    let r53 = Route53Client::new(&config);

    let (instance_name, public_ip) = get_instance_details(&ec2, &args.instance_id)
        .await
        .unwrap_or(("".to_string(), "".to_string()));

    let resp = ec2.terminate_instances()
        .instance_ids(&args.instance_id)
        .send()
        .await?;

    let terminating_instance = resp.terminating_instances()
        .first()
        .and_then(|instance| instance.instance_id())
        .unwrap_or("<unknown>");

    println!("Terminating instance: {}", terminating_instance);

    if let Err(e) = wait_for_instance_termination(&ec2, &args.instance_id).await {
        eprintln!("Warning: failed waiting for terminated state: {}", e);
    }

    if !instance_name.is_empty() && !public_ip.is_empty() {
        let fqdn = format!("{}.ui.rancher.space", instance_name);

        match delete_dns_record(&r53, &args.hosted_zone_id, &fqdn, &public_ip).await {
            Ok(_) => println!("Deleted DNS record: {}", fqdn),
            Err(e) => eprintln!("Failed to delete DNS record {}: {}", fqdn, e),
        }
    }

    if !instance_name.is_empty() {
        let sg_name = format!("roa-{}", instance_name);

        match delete_security_group(&ec2, &args.vpc_id, &sg_name).await {
            Ok(_) => println!("Deleted security group: {}", sg_name),
            Err(e) => eprintln!("Failed to delete security group {}: {}", sg_name, e),
        }
    }

    Ok(())
}

async fn get_instance_details(ec2: &Client, instance_id: &str) -> Result<(String, String), Box<dyn Error>> {
    let desc = ec2
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await?;

    let instance = desc
        .reservations()
        .first()
        .and_then(|reservation| reservation.instances().first())
        .ok_or_else(|| format!("Instance {} not found", instance_id))?;

    let name = instance
        .tags()
        .iter()
        .find(|t| t.key() == Some("Name"))
        .and_then(|t| t.value())
        .unwrap_or("")
        .to_string();

    let ip = instance
        .public_ip_address()
        .unwrap_or("")
        .to_string();

    Ok((name, ip))
}

async fn wait_for_instance_termination(ec2: &Ec2Client, instance_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    for _ in 0..30 {
        println!("Waiting for instance {} to terminate...", instance_id);

        let resp = ec2
            .describe_instances()
            .instance_ids(instance_id)
            .send()
            .await?;

        if let Some(res) = resp.reservations().first()
            && let Some(inst) = res.instances().first()
                && let Some(state) = inst.state().and_then(|s| s.name())
                    && *state == aws_sdk_ec2::types::InstanceStateName::Terminated {
                        return Ok(());
                    }

        sleep(Duration::from_secs(5)).await;
    }

    Err(format!("Instance {} did not reach terminated state", instance_id).into())
}

async fn delete_dns_record(r53: &aws_sdk_route53::Client , hosted_zone_id: &str, fqdn: &str, ip: &str) -> Result<(), Box<dyn std::error::Error>> {
    let resource_record = ResourceRecord::builder().value(ip).build();

    let resource_record_set = ResourceRecordSet::builder()
        .name(fqdn)
        .r#type(RrType::A)
        .ttl(300)
        .resource_records(resource_record?)
        .build();

    let change = Change::builder()
        .action(ChangeAction::Delete)
        .resource_record_set(resource_record_set?)
        .build();

    let batch = ChangeBatch::builder()
        .changes(change?)
        .build();

    r53.change_resource_record_sets()
        .hosted_zone_id(hosted_zone_id)
        .change_batch(batch?)
        .send()
        .await?;

    Ok(())
}

async fn delete_security_group(ec2: &aws_sdk_ec2::Client, vpc_id: &str, group_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let resp = ec2
        .describe_security_groups()
        .filters(
            Filter::builder()
                .name("group-name")
                .values(group_name)
                .build(),
        )
        .filters(
            Filter::builder()
                .name("vpc-id")
                .values(vpc_id)
                .build(),
        )
        .send()
        .await?;

    if let Some(security_group) = resp.security_groups().first()
        && let Some(id) = security_group.group_id() {
            ec2.delete_security_group().group_id(id).send().await?;
        }

    Ok(())
}
