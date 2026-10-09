use std::error::Error;
use std::time::Duration;
use aws_config::{BehaviorVersion, Region};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_ec2::{Client as Ec2Client, Client};
use aws_sdk_ec2::error::ProvideErrorMetadata;
use aws_sdk_route53::Client as Route53Client;
use aws_sdk_ec2::types::Filter;
use aws_sdk_route53::types::{ResourceRecord, ResourceRecordSet, RrType, Change, ChangeAction, ChangeBatch};
use clap::{Parser};
use tokio::time::sleep;
use crate::aws::DEFAULT_REGION;
use crate::instance::{load_instances, manifest_path, save_instances, Instance};

#[derive(Parser, Debug)]
pub struct TerminateArgs {
    #[clap(long, help = "EC2 instance ID to terminate")]
    instance_id: String,

    #[arg(long, env = "ROA_HOSTED_ZONE_ID", help = "Route 53 hosted zone containing the DNS record", hide_env = true)]
    hosted_zone_id: String,

    #[arg(long, env = "ROA_VPC_ID", help = "VPC used to locate the security group for deletion", hide_env = true)]
    vpc_id: String,

    #[arg(long, default_value_t = false, help = "Terminate even if the instance is marked protected in the manifest")]
    force: bool,
}

pub fn termination_blocked(instances: &[Instance], instance_id: &str, force: bool) -> bool {
    !force && instances.iter().any(|i| i.instance_id == instance_id && i.protected)
}

/// The region an instance was provisioned into, if the manifest knows it.
///
/// Kept separate from building the client so the lookup is testable on its own: which region a
/// terminate targets is the difference between deleting the box and deleting nothing while
/// reporting success.
pub fn manifest_region(instances: &[Instance], instance_id: &str) -> Option<String> {
    instances
        .iter()
        .find(|instance| instance.instance_id == instance_id)
        .map(|instance| instance.region.clone())
}

pub async fn terminate(args: TerminateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let manifest_path = manifest_path();
    let instances = load_instances(&manifest_path)?;

    if termination_blocked(&instances, &args.instance_id, args.force) {
        let name = instances
            .iter()
            .find(|i| i.instance_id == args.instance_id)
            .map(|i| i.name.as_str())
            .unwrap_or("<unknown>");

        return Err(format!(
            "{} ({}) is marked protected in the manifest. Re-run with --force to terminate it anyway.",
            args.instance_id, name
        )
        .into());
    }

    // Terminate in the instance's own region, the same way `maintain` and `upgrade` read it.
    // Deleting is the one command where guessing the region wrong is not a failed lookup you
    // notice: it finds no instance, and the security group and DNS record are left behind for a
    // box that is still running somewhere else. An id absent from the manifest has no recorded
    // region and falls back to the ambient config.
    let region_provider = match manifest_region(&instances, &args.instance_id) {
        Some(region) => RegionProviderChain::first_try(Region::new(region)),
        None => RegionProviderChain::default_provider().or_else(Region::new(DEFAULT_REGION)),
    };

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;

    let ec2 = Ec2Client::new(&config);
    let r53 = Route53Client::new(&config);

    let manifest_entry = instances.iter().find(|i| i.instance_id == args.instance_id);

    // EC2 forgets a terminated instance after about an hour, so a box torn down outside roa (or by
    // a terminate that died before saving the manifest) describes as empty. Fall back to what the
    // manifest recorded so the DNS record and security group can still be found.
    let (instance_name, public_ip) = match get_instance_details(&ec2, &args.instance_id).await {
        Ok(details) => details,
        Err(_) => manifest_entry
            .map(|i| (i.name.clone(), i.public_ip.clone()))
            .unwrap_or_default(),
    };

    match ec2.terminate_instances().instance_ids(&args.instance_id).send().await {
        Ok(resp) => {
            let terminating_instance = resp.terminating_instances()
                .first()
                .and_then(|instance| instance.instance_id())
                .unwrap_or("<unknown>");

            println!("Terminating instance: {}", terminating_instance);

            if let Err(e) = wait_for_instance_termination(&ec2, &args.instance_id).await {
                eprintln!("Warning: failed waiting for terminated state: {}", e);
            }
        }
        // Only an id the manifest placed in this region is known to be gone. Without a manifest
        // entry the region was a guess, and "not found" may just mean the box runs somewhere else.
        Err(e) if manifest_entry.is_some()
            && e.as_service_error().and_then(|se| se.code()) == Some("InvalidInstanceID.NotFound") =>
        {
            println!(
                "Instance {} no longer exists; cleaning up its DNS record, security group and manifest entry",
                args.instance_id
            );
        }
        Err(e) => {
            return Err(format!(
                "Failed to terminate {}: {}",
                args.instance_id,
                aws_sdk_ec2::error::DisplayErrorContext(&e)
            )
            .into());
        }
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

    let mut instances = instances;
    let removed_idx = instances
        .iter()
        .position(|instance| instance.instance_id == args.instance_id);

    if let Some(idx) = removed_idx {
        instances.remove(idx);
        save_instances(&manifest_path, &instances)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(id: &str, name: &str, protected: bool) -> Instance {
        Instance {
            instance_id: id.to_string(),
            name: name.to_string(),
            public_ip: "203.0.113.1".to_string(),
            fqdn: format!("{}.ui.rancher.space", name),
            security_group_id: "sg-0".to_string(),
            hosted_zone_id: "Z0".to_string(),
            region: "us-west-2".to_string(),
            created_at: "2026-08-13 21:46:39 UTC".to_string(),
            rancher_repo: None,
            protected,
        }
    }

    // The manifest is what a terminate steers by, so an entry provisioned into another region has
    // to come back as that region and not the caller's.
    #[test]
    fn region_comes_from_the_matching_manifest_entry() {
        let mut far = instance("i-far", "far", false);
        far.region = "eu-north-1".to_string();
        let instances = vec![instance("i-near", "near", false), far];

        assert_eq!(
            manifest_region(&instances, "i-far").as_deref(),
            Some("eu-north-1")
        );
        assert_eq!(
            manifest_region(&instances, "i-near").as_deref(),
            Some("us-west-2")
        );
    }

    // An id roa never provisioned has no recorded region; the caller's ambient config is the only
    // thing left to go on, so the absence has to be distinguishable from a region.
    #[test]
    fn an_instance_absent_from_the_manifest_has_no_region() {
        let instances = vec![instance("i-known", "known", false)];

        assert_eq!(manifest_region(&instances, "i-unknown"), None);
    }

    #[test]
    fn protected_instance_is_blocked() {
        let instances = vec![instance("i-shared", "shared", true)];
        assert!(termination_blocked(&instances, "i-shared", false));
    }

    #[test]
    fn force_overrides_protection() {
        let instances = vec![instance("i-shared", "shared", true)];
        assert!(!termination_blocked(&instances, "i-shared", true));
    }

    #[test]
    fn unprotected_instance_is_not_blocked() {
        let instances = vec![instance("i-throwaway", "prak-throwaway", false)];
        assert!(!termination_blocked(&instances, "i-throwaway", false));
    }

    #[test]
    fn instance_absent_from_manifest_is_not_blocked() {
        // The guard defends what roa records and nothing else. Keeping this as a test rather than
        // a comment so the limitation cannot be quietly forgotten.
        let instances = vec![instance("i-shared", "shared", true)];
        assert!(!termination_blocked(&instances, "i-unknown", false));
    }

    #[test]
    fn protection_is_matched_by_id_not_name() {
        let instances = vec![
            instance("i-shared", "shared", true),
            instance("i-throwaway", "shared-old-k3d", false),
        ];
        assert!(!termination_blocked(&instances, "i-throwaway", false));
    }
}
