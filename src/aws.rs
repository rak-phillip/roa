use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client as Ec2Client;

/// Builds an EC2 client for `region`.
///
/// The ambient AWS config still wins; the argument is only the fallback. Callers pass the region
/// recorded on the manifest entry so a command always targets the instance's own region rather
/// than whatever the caller's environment happens to say.
pub async fn ec2_client(region: &str) -> Ec2Client {
    let region_provider =
        RegionProviderChain::default_provider().or_else(Region::new(region.to_string()));

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;

    Ec2Client::new(&config)
}

/// Resolves an instance's root EBS volume.
///
/// Both reading snapshots (`maintain`) and creating one (`upgrade`) need this, and neither should
/// guess: the root device is whatever the instance says it is, with `/dev/sda1` only as a fallback
/// for an instance that does not report one.
pub async fn root_volume_id(
    ec2: &Ec2Client,
    instance_id: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let described = ec2
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await?;

    let root_device = described
        .reservations()
        .first()
        .and_then(|r| r.instances().first())
        .ok_or_else(|| format!("instance {} not found", instance_id))?;

    let root_name = root_device.root_device_name().unwrap_or("/dev/sda1");

    let volume_id = root_device
        .block_device_mappings()
        .iter()
        .find(|m| m.device_name() == Some(root_name))
        .and_then(|m| m.ebs())
        .and_then(|e| e.volume_id())
        .ok_or("could not resolve the root volume")?;

    Ok(volume_id.to_string())
}

use aws_sdk_ec2::types::{ResourceType, Tag, TagSpecification};
use std::time::{Duration, Instant};

/// Creates a snapshot of `volume_id` and returns its id.
pub async fn create_snapshot(
    ec2: &Ec2Client,
    volume_id: &str,
    description: &str,
    tags: &[(String, String)],
) -> Result<String, Box<dyn std::error::Error>> {
    let mut spec = TagSpecification::builder().resource_type(ResourceType::Snapshot);
    for (key, value) in tags {
        spec = spec.tags(Tag::builder().key(key).value(value).build());
    }

    let created = ec2
        .create_snapshot()
        .volume_id(volume_id)
        .description(description)
        .tag_specifications(spec.build())
        .send()
        .await?;

    Ok(created
        .snapshot_id()
        .ok_or("CreateSnapshot returned no snapshot id")?
        .to_string())
}

/// Polls until the snapshot completes, or the deadline passes.
///
/// A fixed interval against a wall-clock deadline, deliberately not the iteration-capped
/// exponential backoff used elsewhere in this crate: that tops out around thirteen minutes, and
/// observed snapshots of this volume have taken 11, 12, 20, 34 and 44 minutes with no relationship
/// to how much changed. `aws ec2 wait snapshot-completed` is no use either -- it gives up at ten
/// minutes and exits 255, which reads like a failure and is not one.
pub async fn wait_for_snapshot(
    ec2: &Ec2Client,
    snapshot_id: &str,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut last_progress = String::new();
    let mut announced = false;

    loop {
        let described = ec2
            .describe_snapshots()
            .snapshot_ids(snapshot_id)
            .send()
            .await?;

        let snapshot = described
            .snapshots()
            .first()
            .ok_or_else(|| format!("snapshot {} not found", snapshot_id))?;

        let state = snapshot.state().map(|s| s.as_str()).unwrap_or("unknown");
        let progress = snapshot.progress().unwrap_or("").to_string();

        match state {
            "completed" => {
                println!("  snapshot {} completed in {}", snapshot_id, elapsed(started));
                return Ok(());
            }
            "error" => {
                let message = snapshot.state_message().unwrap_or("no detail");
                return Err(format!("snapshot {} failed: {}", snapshot_id, message).into());
            }
            _ => {}
        }

        if !announced {
            // Without this the first person to run it kills the command at minute eight.
            println!(
                "  waiting for {} -- AWS reports progress in coarse jumps, so 0% for several \
                 minutes is normal and not a stall",
                snapshot_id
            );
            announced = true;
        } else if progress != last_progress {
            println!("  snapshot {} at {} ({})", snapshot_id, progress, elapsed(started));
        }
        last_progress = progress;

        if started.elapsed() >= timeout {
            return Err(format!(
                "snapshot {} has not completed after {}. It is still running and may yet \
                 complete -- check it before assuming it failed, and do not re-take it.",
                snapshot_id,
                elapsed(started)
            )
            .into());
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn elapsed(since: Instant) -> String {
    let secs = since.elapsed().as_secs();
    format!("{}m{:02}s", secs / 60, secs % 60)
}
