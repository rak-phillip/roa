use std::time::Duration;
use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{BlockDeviceMapping, EbsBlockDevice, Tag, TagSpecification, InstanceNetworkInterfaceSpecification, InstanceType};
use clap::{Parser, ValueEnum};
use base64::{engine::general_purpose, Engine};
use chrono::Utc;
use crate::instance::{load_instances, manifest_path, save_instances, Instance};
use crate::network::{create_security_group, get_public_ip, upsert_dns_record};

#[derive(Debug, Clone)]
pub enum RancherRepo {
    Latest,
    Prime,
    PrimeLatest,
    PrimeAlpha,
    CommunityAlpha,
    ReleaseLine(String),
}

impl std::str::FromStr for RancherRepo {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "latest" => Ok(RancherRepo::Latest),
            "prime" => Ok(RancherRepo::Prime),
            "prime-latest" => Ok(RancherRepo::PrimeLatest),
            // `alpha` kept as a backward-compatible alias for `prime-alpha`,
            // which is where it has always pointed.
            "alpha" | "prime-alpha" => Ok(RancherRepo::PrimeAlpha),
            "community-alpha" => Ok(RancherRepo::CommunityAlpha),
            _ if s.starts_with("release-") => Ok(RancherRepo::ReleaseLine(s.to_string())),
            _ => Err(format!("Invalid rancher repo: {}", s)),
        }
    }
}

impl RancherRepo {
    pub fn value(&self) -> String {
        match &self {
            RancherRepo::Latest => "https://releases.rancher.com/server-charts/latest".to_string(),
            RancherRepo::Prime => "https://charts.rancher.com/server-charts/prime".to_string(),
            RancherRepo::PrimeLatest => "https://charts.optimus.rancher.io/server-charts/latest".to_string(),
            RancherRepo::PrimeAlpha => "https://charts.optimus.rancher.io/server-charts/alpha".to_string(),
            RancherRepo::CommunityAlpha => "https://releases.rancher.com/server-charts/alpha".to_string(),
            RancherRepo::ReleaseLine(line) => {
                format!("https://charts.optimus.rancher.io/server-charts/{}", line)
            }
        }
    }
}

#[derive(Debug, Clone, ValueEnum)]
enum ProvisionMode {
    Helm,
    Docker,
}

#[derive(Parser, Debug)]
pub struct ProvisionArgs {
    #[arg(long = "name", help = "Instance name. Also used as the subdomain: `<name>.ui.rancher.space`")]
    name: String,

    #[arg(long, value_enum, default_value_t = ProvisionMode::Helm, help = "Install method: `helm` (k3s + Helm) or `docker`")]
    mode: ProvisionMode,

    #[arg(long, default_value_t = 64, help = "EBS root volume size in GB")]
    storage_gb: i32,

    #[arg(long, env = "ROA_VPC_ID", help = "VPC to launch the instance into", hide_env = true)]
    vpc_id: String,

    #[arg(long, env = "ROA_SUBNET_ID", help = "Subnet to attach the instance to", hide_env = true)]
    subnet_id: String,

    #[arg(long, help = "EC2 key pair name for SSH access")]
    key_name: String,

    #[arg(long, env = "ROA_SECURITY_GROUP_ID", help = "Use an existing security group instead of creating one", hide_env = true)]
    security_group_id: Option<String>,

    #[arg(long, env = "ROA_HOSTED_ZONE_ID", help = "Route 53 hosted zone ID for DNS management", hide_env = true)]
    hosted_zone_id: String,

    #[arg(long, help = "Email address for Let's Encrypt certificate issuance")]
    email: String,

    #[arg(long, default_value = "latest", help = "Rancher Helm chart repo: `latest`, `prime`, `prime-latest` (Prime RC and head), `prime-alpha` (alias `alpha`), `community-alpha`, or release-<major>-<minor>")]
    rancher_repo: RancherRepo,

    #[arg(long, help = "Pin a specific Rancher version (e.g. `v2.14.0`)")]
    rancher_version: Option<String>,

    #[arg(long, default_value = "rancher/rancher", help = "Docker image registry (Docker mode only)")]
    docker_registry: String,

    #[arg(long, help = "Override the Rancher hostname")]
    rancher_hostname: Option<String>,

    #[arg(long, alias="password", help = "Set the Rancher bootstrap password")]
    rancher_bootstrap_password: Option<String>,

    #[arg(long, help = "Pin a specific k3s version (e.g. `v1.36.2+k3s1`). Defaults per Rancher minor; falls back to the k3s installer's latest stable when the Rancher version is unknown or unpinned.")]
    k3s_version: Option<String>,

    #[arg(long, env = "ROA_AMI_ID", help = "AMI ID to use (Ubuntu-based recommended)", hide_env = true)]
    ami_id: String,

    #[arg(long, default_value_t = false, help = "Block until DNS propagates and Rancher is reachable")]
    wait_for_ready: bool,
}

// Maps Rancher minor version to the highest k3s version certified by Rancher's support matrix.
// Returns the k3s release version (e.g. `v1.36.2+k3s1`).
// Source: https://www.suse.com/suse-rancher/support-matrix/
fn default_k3s_version(rancher_version: &str) -> Option<&'static str> {
    let stripped = rancher_version.trim_start_matches('v');
    let minor = stripped.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    match minor.as_str() {
        "2.11" => Some("v1.32.3+k3s1"),
        "2.12" => Some("v1.33.3+k3s1"),
        "2.13" => Some("v1.34.3+k3s1"),
        "2.14" => Some("v1.35.5+k3s1"),
        "2.15" => Some("v1.36.2+k3s1"),
        "2.16" => Some("v1.36.3+k3s1"),
        _ => None,
    }
}

// Reduces a pinned k3s release to the k3s release channel for its minor:
// `v1.36.3+k3s1` -> `v1.36`. Returns None for anything that isn't a pinned `vX.Y.Z` version,
// which is the signal to write no upgrade Plan at all rather than guess at a channel.
fn k3s_minor_channel(k3s_version: &str) -> Option<String> {
    let stripped = k3s_version.trim().trim_start_matches('v');
    let mut parts = stripped.split('.');

    let major = parts.next().filter(|p| !p.is_empty())?;
    let minor = parts.next().filter(|p| !p.is_empty())?;

    // Anything after the minor must look like a patch, so a bare `v1.36` or a stray
    // `stable` never silently becomes a channel.
    let patch = parts.next()?;
    if !patch.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }

    if !major.chars().all(|c| c.is_ascii_digit()) || !minor.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    Some(format!("v{}.{}", major, minor))
}

// Renders the shell that applies a system-upgrade-controller Plan tracking `channel`.
// The channel is always minor-pinned; see the comment in `user-data` for why.
fn k3s_upgrade_plan(channel: &str) -> String {
    format!(
        r#"kubectl apply -f - <<'PLANEOF'
apiVersion: upgrade.cattle.io/v1
kind: Plan
metadata:
  name: k3s-server
  namespace: system-upgrade
spec:
  concurrency: 1
  channel: https://update.k3s.io/v1-release/channels/{channel}
  serviceAccountName: system-upgrade
  cordon: true
  nodeSelector:
    matchExpressions:
      - {{ key: node-role.kubernetes.io/control-plane, operator: In, values: ["true"] }}
  upgrade:
    image: rancher/k3s-upgrade
PLANEOF"#
    )
}

pub async fn provision(args: ProvisionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-west-2".to_string());

    let region_provider = RegionProviderChain::default_provider()
        .or_else(Region::new(region.clone()));

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);
    let r53 = aws_sdk_route53::Client::new(&config);

    let rancher_version = match &args.rancher_version {
        Some(version) => format!("--version {}", version),
        None => "--devel".to_string(),
    };

    let rancher_repo = &args.rancher_repo.value();

    let fqdn = format!("{}.ui.rancher.space", args.name);

    let bootstrap_password_flag = match &args.rancher_bootstrap_password {
        Some(password) => format!("--set bootstrapPassword=\"{}\"", password),
        None => String::new(),
    };

    let k3s_version = match args.k3s_version.as_deref()
        .or_else(|| args.rancher_version.as_deref().and_then(default_k3s_version))
    {
        Some(version) => {
            println!("Using k3s version: {}", version);
            version.to_string()
        }
        None => {
            println!("No k3s version pinned; the k3s installer will use the latest stable release");
            String::new()
        }
    };

    let k3s_upgrade_plan = match k3s_minor_channel(&k3s_version) {
        Some(channel) => {
            println!("Pinning the k3s upgrade Plan to channel: {}", channel);
            k3s_upgrade_plan(&channel)
        }
        None => {
            println!("No k3s minor to pin; writing no k3s upgrade Plan");
            String::new()
        }
    };

    let user_data_script = match args.mode {
        ProvisionMode::Helm => &*include_str!("../user-data")
            .replace("\"<RANCHER_HOSTNAME>\"", &args.rancher_hostname.unwrap_or(fqdn.clone()))
            .replace("\"<LETS_ENCRYPT_EMAIL>\"", &args.email)
            .replace("\"<RANCHER_REPO>\"", rancher_repo)
            .replace("\"<RANCHER_VERSION>\"", &rancher_version)
            .replace("\"<RANCHER_BOOTSTRAP_PASSWORD>\"", bootstrap_password_flag.as_str())
            .replace("\"<K3S_VERSION>\"", &k3s_version)
            .replace("\"<K3S_UPGRADE_PLAN>\"", &k3s_upgrade_plan),
        ProvisionMode::Docker => {
            let version = args.rancher_version
                .as_deref()
                .unwrap_or("head");

            &*include_str!("../user-data-docker")
                .replace("\"<DOCKER_REGISTRY>\"", &args.docker_registry)
                .replace("\"<RANCHER_VERSION>\"", version)
        },
    };
    let user_data = general_purpose::STANDARD.encode(user_data_script);

    let block_device = BlockDeviceMapping::builder()
        .device_name("/dev/sda1")
        .ebs(
            EbsBlockDevice::builder()
                .volume_size(args.storage_gb)
                .volume_type(aws_sdk_ec2::types::VolumeType::Gp3)
                .delete_on_termination(true)
                .build()
        )
        .build();

    let name_tag = Tag::builder()
        .key("Name")
        .value(args.name.clone())
        .build();

    let tag_spec = TagSpecification::builder()
        .resource_type(aws_sdk_ec2::types::ResourceType::Instance)
        .tags(name_tag)
        .build();

    let security_group_id = match args.security_group_id {
        Some(id) => id,
        None => create_security_group(&client, &args.vpc_id, &args.name).await?,
    };

    let network_interface = InstanceNetworkInterfaceSpecification::builder()
        .associate_public_ip_address(true)
        .subnet_id(args.subnet_id.clone())
        .groups(security_group_id.clone())
        .device_index(0)
        .build();

    let resp = client
        .run_instances()
        .image_id(args.ami_id)
        .instance_type(InstanceType::T32xlarge)
        .min_count(1)
        .max_count(1)
        .key_name(args.key_name.clone())
        .user_data(user_data)
        .network_interfaces(network_interface)
        .block_device_mappings(block_device)
        .tag_specifications(tag_spec)
        .send()
        .await?;

    let instance_id = resp.instances()
        .first()
        .and_then(|instance| instance.instance_id())
        .unwrap_or("<unknown>");

    println!("Launched instance: {}", instance_id);

    let public_ip = get_public_ip(&client, instance_id).await?;
    println!("Public IP: {}", public_ip);

    let change_id = upsert_dns_record(&r53, &args.hosted_zone_id, &fqdn, &public_ip).await?;

    if args.wait_for_ready {
        let url = format!("https://{}.ui.rancher.space/", args.name);
        wait_for_dns(&r53, &change_id).await?;
        wait_for_rancher(&url, Duration::from_secs(600)).await?;
    }

    let provisioned_instance = Instance {
        instance_id: String::from(instance_id),
        name: args.name,
        created_at: Utc::now().to_string(),
        hosted_zone_id: args.hosted_zone_id,
        public_ip,
        fqdn,
        security_group_id,
        region,
        rancher_repo: Some(rancher_repo.to_string()),
    };

    let manifest_path = manifest_path();

    let mut instances = load_instances(&manifest_path)?;

    instances.push(provisioned_instance);

    save_instances(&manifest_path, &instances)?;

    Ok(())
}

async fn wait_for_dns(r53: &aws_sdk_route53::Client, change_id: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut delay = Duration::from_secs(2);

    for _ in 0..20 {
        let resp = r53.get_change().id(change_id).send().await?;
        let status = resp
            .change_info()
            .map(|ci| ci.status())
            .unwrap_or(&aws_sdk_route53::types::ChangeStatus::Pending);

        if matches!(status, &aws_sdk_route53::types::ChangeStatus::Insync) {
            println!("DNS change status: {:?}", status);
            return Ok(());
        }

        println!("Waiting for DNS to become INSYNC...");

        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_secs(30));
    }

    Err("DNS change did not become INSYNC".into())
}

async fn wait_for_rancher(url: &str, timeout: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()?;

    let start = std::time::Instant::now();
    let mut delay = Duration::from_secs(5);

    while start.elapsed() < timeout {
        match client.get(url).send().await {
            Ok(resp) => {
                if resp.status().is_success() || resp.status().is_redirection() {
                    println!("Rancher is ready at {}", url);
                    return Ok(());
                }
            }
            Err(e) => {
                eprintln!("Rancher not ready yet ({}): {}", url, e);
            }
        }
        tokio::time::sleep(delay).await;
        delay = std::cmp::min(delay * 2, Duration::from_secs(60));
    }

    Err(format!("Rancher did not become ready at {}", url).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduces_a_pinned_k3s_release_to_its_minor_channel() {
        assert_eq!(k3s_minor_channel("v1.36.3+k3s1").as_deref(), Some("v1.36"));
        assert_eq!(k3s_minor_channel("1.35.5+k3s1").as_deref(), Some("v1.35"));
        assert_eq!(k3s_minor_channel(" v1.32.3+k3s1 ").as_deref(), Some("v1.32"));
    }

    #[test]
    fn refuses_to_guess_a_channel_from_an_unpinned_version() {
        // An empty version is what `provision` passes when nothing is pinned; writing a Plan
        // then would guess at a channel.
        assert_eq!(k3s_minor_channel(""), None);
        assert_eq!(k3s_minor_channel("v1.36"), None);
        assert_eq!(k3s_minor_channel("stable"), None);
        assert_eq!(k3s_minor_channel("latest"), None);
        assert_eq!(k3s_minor_channel("v1.x.3"), None);
    }

    #[test]
    fn every_default_k3s_version_yields_a_channel() {
        for rancher in ["2.11", "2.12", "2.13", "2.14", "2.15", "2.16"] {
            let k3s = default_k3s_version(rancher).expect("mapped");
            assert!(
                k3s_minor_channel(k3s).is_some(),
                "{} -> {} has no channel",
                rancher,
                k3s
            );
        }
    }

    #[test]
    fn the_plan_pins_the_channel_and_never_a_floating_one() {
        let plan = k3s_upgrade_plan("v1.36");
        assert!(plan.contains("https://update.k3s.io/v1-release/channels/v1.36"));
        assert!(!plan.contains("channels/stable"));
        assert!(!plan.contains("channels/latest"));
        assert!(plan.contains("namespace: system-upgrade"));
        assert!(plan.contains("image: rancher/k3s-upgrade"));
    }

    #[test]
    fn user_data_carries_the_plan_placeholder() {
        assert!(include_str!("../user-data").contains("\"<K3S_UPGRADE_PLAN>\""));
    }

    #[test]
    fn rendering_user_data_substitutes_or_removes_the_plan() {
        let template = include_str!("../user-data");

        let channel = k3s_minor_channel("v1.36.3+k3s1").unwrap();
        let with_plan = template.replace("\"<K3S_UPGRADE_PLAN>\"", &k3s_upgrade_plan(&channel));
        assert!(with_plan.contains("kind: Plan"));
        assert!(with_plan.contains("channels/v1.36"));
        assert!(!with_plan.contains("<K3S_UPGRADE_PLAN>"));
        // The Plan is applied as a heredoc; an unbalanced marker would break provisioning.
        assert_eq!(with_plan.matches("PLANEOF").count(), 2);

        // Unpinned k3s leaves no Plan behind at all.
        let without_plan = template.replace("\"<K3S_UPGRADE_PLAN>\"", "");
        assert!(!without_plan.contains("kind: Plan"));
        assert!(!without_plan.contains("<K3S_UPGRADE_PLAN>"));
    }
}
