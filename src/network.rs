use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{IpPermission, IpRange};
use std::time::Duration;
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
    for _ in 0..30 {
        println!("Getting public IP for {}", instance_id);

        let resp = client.describe_instances()
            .instance_ids(instance_id)
            .send()
            .await?;

        if let Some(reservation) = resp.reservations().first() {
            if let Some(instance) = reservation.instances().first() {
                if let Some(ip) = instance.public_ip_address() {
                    return Ok(ip.to_string());
                }
            }
        }

        sleep(Duration::from_secs(5)).await;
    }

    Err("No public IP found".into())
}
