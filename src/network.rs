use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{IpPermission, IpRange};
use aws_sdk_route53 as route53;
use route53::types::{Change, ChangeAction, ChangeBatch, ResourceRecord, ResourceRecordSet, RrType};
use std::time::Duration;
use aws_sdk_ec2::error::ProvideErrorMetadata;
use tokio::time::sleep;

pub async fn create_security_group( client: &Client, vpc_id: &str, name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let security_group = client.create_security_group()
        .group_name(format!("roa-{}", name).to_string())
        .description("ROA: Rancher development security group")
        .vpc_id(vpc_id)
        .send()
        .await?;

    let security_group_id = security_group
        .group_id
        .ok_or("No security group ID specified")?;

    let ip_range = IpRange::builder()
        .cidr_ip("0.0.0.0/0")
        .build();

    let ssh = IpPermission::builder()
        .ip_protocol("tcp")
        .from_port(22)
        .to_port(22)
        .ip_ranges(ip_range.clone())
        .build();

    let http = IpPermission::builder()
        .ip_protocol("tcp")
        .from_port(80)
        .to_port(80)
        .ip_ranges(ip_range.clone())
        .build();

    let https = IpPermission::builder()
        .ip_protocol("tcp")
        .from_port(443)
        .to_port(443)
        .ip_ranges(ip_range.clone())
        .build();

    client.authorize_security_group_ingress()
        .group_id(&security_group_id)
        .ip_permissions(ssh)
        .ip_permissions(https)
        .ip_permissions(http)
        .send()
        .await?;

    Ok(security_group_id)
}

pub async fn get_public_ip(client: &Client, instance_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut delay = Duration::from_secs(2);

    for _ in 0..30 {
        println!("Getting public IP for {}", instance_id);

        match client
            .describe_instances()
            .instance_ids(instance_id)
            .send()
            .await
        {
            Ok(resp) => {
                if let Some(reservation) = resp.reservations().first() {
                    if let Some(instance) = reservation.instances().first() {
                        if let Some(ip) = instance.public_ip_address() {
                            return Ok(ip.to_string());
                        }
                    }
                }
            }

            Err(sdk_error) => {
                if let Some(service_error) = sdk_error.as_service_error() {
                    if service_error.code() == Some("InvalidInstanceID.NotFound") {
                        eprintln!("Instance {} not found; retrying...", instance_id);
                    } else {
                        return Err(sdk_error.into());
                    }
                } else {
                    return Err(sdk_error.into())
                }
            }
        }

        sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_secs(30));
    }

    Err("No public IP found".into())
}

pub async fn upsert_dns_record(r53: &route53::Client, hosted_zone_id: &str, fqdn: &str, ip: &str) -> Result<(), Box<dyn std::error::Error>> {
    let resource_record = ResourceRecord::builder()
        .value(ip)
        .build();

    let resource_record_set = ResourceRecordSet::builder()
        .name(fqdn.to_string())
        .r#type(RrType::A)
        .ttl(300)
        .resource_records(resource_record?)
        .build();

    let change = Change::builder()
        .action(ChangeAction::Upsert)
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
