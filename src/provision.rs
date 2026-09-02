use std::time::Duration;
use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{IamInstanceProfileSpecification, BlockDeviceMapping, EbsBlockDevice, Tag, TagSpecification, InstanceNetworkInterfaceSpecification, InstanceType};
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

/// A port to publish on the container and open in the security group.
///
/// `host` is the side the outside world reaches and so the side the security
/// group governs; `container` is where it lands inside. They default to the
/// same number, which is not merely a convenience: a NodePort has no way to
/// learn that the host publishes it as something else, so anything reading the
/// cluster to build a URL -- a UI extension, say -- can only be right when the
/// two agree. Map them apart for anything that is not a NodePort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortMapping {
    host: u16,
    container: u16,
}

impl PortMapping {
    /// The side the outside world connects to, and so the side a security group
    /// rule is written for.
    pub fn host(&self) -> u16 {
        self.host
    }
}

/// Ports the instance already publishes or allows before any of these are added.
/// SSH is in the security group but not published by the container.
const RESERVED_HOST_PORTS: [u16; 3] = [22, 80, 443];

// The `--ssh-cidr` value that means "wherever I am right now".
const SSH_CIDR_SELF: &str = "self";

// Resolves `self` to the caller's current public address as a /32; passes anything else through
// unchanged, since `parse_cidr` has already validated it.
//
// A literal /32 written into `roa_variables` rots the first time a residential address rotates,
// and it rots silently -- the next instance comes up with a rule that admits nobody, which reads
// as a broken box rather than a stale rule. Resolving at provision time keeps the scoped choice
// the cheap one to make.
async fn resolve_ssh_cidr(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    if !value.eq_ignore_ascii_case(SSH_CIDR_SELF) {
        return Ok(value.to_string());
    }

    let body = reqwest::Client::new()
        .get("https://checkip.amazonaws.com")
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .text()
        .await?;

    let cidr = format!("{}/32", body.trim());
    parse_cidr(&cidr).map_err(|e| format!("checkip.amazonaws.com did not return an address: {}", e))?;

    println!("Scoping SSH to {}", cidr);
    Ok(cidr)
}

// Substitutes the sshd drop-in into a cloud-init template.
//
// Shared by both modes because how exposed sshd is has nothing to do with whether Rancher arrives
// by Helm or by Docker -- the two templates drifting apart is exactly how `--mode docker` came to
// hand out an instance running stock sshd. The placeholder is consumed with its own line so the
// snippet starts at column zero.
fn render_sshd_hardening(template: &str) -> String {
    template.replace(
        "\"<SSHD_HARDENING>\"\n",
        include_str!("../user-data-sshd-hardening"),
    )
}

fn parse_cidr(value: &str) -> Result<String, String> {
    let value = value.trim();

    // Resolved at provision time rather than here: parsing runs once per process, but the address
    // it would bake in outlives the process in `roa_variables`.
    if value.eq_ignore_ascii_case(SSH_CIDR_SELF) {
        return Ok(SSH_CIDR_SELF.to_string());
    }

    let (addr, prefix) = value
        .split_once('/')
        .ok_or_else(|| format!("`{}` is not a CIDR -- it needs a prefix length, e.g. `{}/32`", value, value))?;

    let octets: Vec<&str> = addr.split('.').collect();
    if octets.len() != 4 || !octets.iter().all(|o| !o.is_empty() && o.parse::<u8>().is_ok()) {
        return Err(format!("`{}` is not a dotted-quad IPv4 address in `{}`", addr, value));
    }

    match prefix.parse::<u8>() {
        Ok(bits) if bits <= 32 => Ok(value.to_string()),
        _ => Err(format!("`{}` is not a prefix length between 0 and 32 in `{}`", prefix, value)),
    }
}

fn parse_port_mapping(value: &str) -> Result<PortMapping, String> {
    let (host, container) = value.split_once(':').unwrap_or((value, value));

    let port = |side: &str, raw: &str| -> Result<u16, String> {
        match raw.trim().parse::<u16>() {
            Ok(0) | Err(_) => Err(format!(
                "`{}` is not a port number ({} side of `{}`)",
                raw.trim(),
                side,
                value
            )),
            Ok(port) => Ok(port),
        }
    };

    Ok(PortMapping {
        host: port("host", host)?,
        container: port("container", container)?,
    })
}

/// The mappings actually worth acting on, in the order they were given.
///
/// Drops anything the instance already handles rather than passing it on: a
/// duplicate security group rule is rejected outright by EC2, and a second
/// `-p 443:...` stops the container from starting at all -- both of which would
/// turn a harmless repeated flag into an instance with no Rancher on it.
fn usable_ports(ports: &[PortMapping]) -> Vec<PortMapping> {
    let mut seen: Vec<u16> = RESERVED_HOST_PORTS.to_vec();

    ports
        .iter()
        .filter(|mapping| {
            if seen.contains(&mapping.host) {
                false
            } else {
                seen.push(mapping.host);
                true
            }
        })
        .copied()
        .collect()
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

    #[arg(long, default_value_t = false, help = "Mark the instance protected so `terminate` refuses it without --force")]
    protect: bool,

    #[arg(
        long,
        env = "ROA_SSH_CIDR",
        hide_env = true,
        default_value = "0.0.0.0/0",
        value_parser = parse_cidr,
        help = "CIDR allowed to reach SSH, or `self` for your current public address as a /32. Defaults to anywhere, which is fine for a throwaway and wrong for anything long-lived. Only scopes port 22; 80 and 443 stay open"
    )]
    ssh_cidr: String,

    #[arg(long, env = "ROA_SSM_PROFILE", hide_env = true, help = "Existing IAM instance profile to attach at launch, e.g. for SSM Session Manager access. roa never creates the profile -- it only attaches one you already made")]
    ssm_profile: Option<String>,

    #[arg(long, help = "Pin a specific Rancher version (e.g. `v2.14.0`)")]
    rancher_version: Option<String>,

    #[arg(long, default_value = "rancher/rancher", help = "Docker image registry (Docker mode only)")]
    docker_registry: String,

    #[arg(
        long = "ports",
        value_name = "PORT[:CONTAINER_PORT]",
        value_delimiter = ',',
        value_parser = parse_port_mapping,
        help = "Extra ports to open in the security group, and to publish on the container in Docker mode -- an ingress controller's NodePort, say. Comma-separated and repeatable. `30443` publishes 30443 on both sides; `8080:80` maps host 8080 to container 80. Only applies to a group roa creates; an existing --security-group-id is left alone"
    )]
    ports: Vec<PortMapping>,

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

    let user_data_script = render_sshd_hardening(&match args.mode {
        ProvisionMode::Helm => include_str!("../user-data")
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

            render_docker_user_data(&args.docker_registry, version, &args.ports)
        },
    });
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

    // What an IAM policy scopes against. `ssm:StartSession` cannot be granted on "every instance
    // roa will ever make" by ARN, but it can be granted on a resource tag -- so one policy covers
    // instances that do not exist yet, instead of an edit per box.
    let managed_by_tag = Tag::builder()
        .key("ManagedBy")
        .value("roa")
        .build();

    let tag_spec = TagSpecification::builder()
        .resource_type(aws_sdk_ec2::types::ResourceType::Instance)
        .tags(name_tag)
        .tags(managed_by_tag)
        .build();

    let ssh_cidr = resolve_ssh_cidr(&args.ssh_cidr).await?;

    let security_group_id = match args.security_group_id {
        Some(id) => id,
        None => create_security_group(&client, &args.vpc_id, &args.name, &usable_ports(&args.ports), &ssh_cidr).await?,
    };

    let network_interface = InstanceNetworkInterfaceSpecification::builder()
        .associate_public_ip_address(true)
        .subnet_id(args.subnet_id.clone())
        .groups(security_group_id.clone())
        .device_index(0)
        .build();

    let mut run = client
        .run_instances()
        .image_id(args.ami_id)
        .instance_type(InstanceType::T32xlarge)
        .min_count(1)
        .max_count(1)
        .key_name(args.key_name.clone())
        .user_data(user_data)
        .network_interfaces(network_interface)
        .block_device_mappings(block_device)
        .tag_specifications(tag_spec);

    if let Some(profile) = &args.ssm_profile {
        println!("Attaching IAM instance profile: {}", profile);
        run = run.iam_instance_profile(
            IamInstanceProfileSpecification::builder().name(profile).build(),
        );
    }

    let resp = run.send().await?;

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
        protected: args.protect,
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

/// Render the docker-mode cloud-init, publishing any extra ports asked for.
///
/// The `-p` lines are generated rather than templated because there can be any
/// number of them, and the placeholder is consumed *with its own line* -- an
/// empty replacement that left a blank line behind would end the
/// backslash-continued `docker run` early, and the instance would come up with
/// no Rancher on it at all.
fn render_docker_user_data(registry: &str, version: &str, ports: &[PortMapping]) -> String {
    let publishes = usable_ports(ports)
        .iter()
        .map(|mapping| format!("  -p {}:{} \\\n", mapping.host, mapping.container))
        .collect::<String>();

    include_str!("../user-data-docker")
        .replace("\"<DOCKER_REGISTRY>\"", registry)
        .replace("\"<RANCHER_VERSION>\"", version)
        .replace("  \"<EXTRA_PORTS>\"\n", &publishes)
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

    fn mapping(spec: &str) -> PortMapping {
        parse_port_mapping(spec).expect("a port mapping")
    }

    #[test]
    fn a_bare_port_maps_to_itself() {
        // The form a NodePort has to take: it cannot see the host's mapping, so
        // only matching numbers let a URL built from the cluster work.
        assert_eq!(mapping("30443"), PortMapping { host: 30443, container: 30443 });
        assert_eq!(mapping(" 30443 "), PortMapping { host: 30443, container: 30443 });
    }

    #[test]
    fn a_pair_maps_host_to_container() {
        assert_eq!(mapping("8080:80"), PortMapping { host: 8080, container: 80 });
    }

    #[test]
    fn refuses_anything_that_is_not_a_port() {
        for spec in ["", "0", "http", "8080:", "8080:0", ":80", "70000", "8080:https"] {
            assert!(parse_port_mapping(spec).is_err(), "{} parsed", spec);
        }
    }

    #[test]
    fn drops_ports_the_instance_already_handles() {
        // A repeated rule is rejected by EC2 outright, and a second `-p 443:...`
        // stops the container starting -- so a duplicate must never reach either.
        let asked = vec![
            mapping("443"),
            mapping("30443"),
            mapping("30443"),
            mapping("22"),
            mapping("80:8080"),
            mapping("8080:80"),
        ];

        assert_eq!(usable_ports(&asked), vec![mapping("30443"), mapping("8080:80")]);
    }

    #[test]
    fn docker_user_data_publishes_only_rancher_by_default() {
        let script = render_docker_user_data("rancher/rancher", "head", &[]);

        assert!(script.contains("-p 80:80"));
        assert!(script.contains("-p 443:443"));
        assert!(!script.contains("<EXTRA_PORTS>"));
        // The placeholder took its own line with it, so the run stays one
        // continued command. A blank line here would truncate it at :443.
        assert!(script.contains("  -p 443:443 \\\n  -e CATTLE_BOOTSTRAP_PASSWORD"));
    }

    #[test]
    fn docker_user_data_publishes_every_port_asked_for() {
        let script = render_docker_user_data(
            "rancher/rancher",
            "head",
            &[mapping("30443"), mapping("8080:80")],
        );

        assert!(script.contains("  -p 30443:30443 \\\n"));
        assert!(script.contains("  -p 8080:80 \\\n"));
        assert!(script.contains("  -p 443:443 \\\n  -p 30443:30443"));
        assert!(script.contains("-e CATTLE_BOOTSTRAP_PASSWORD"));
    }

    #[test]
    fn docker_user_data_never_leaves_a_blank_line_in_the_run() {
        for ports in [vec![], vec![mapping("30443")]] {
            let script = render_docker_user_data("rancher/rancher", "head", &ports);
            let run = script
                .split("sudo docker run -d")
                .nth(1)
                .expect("a docker run block");
            let block: Vec<&str> = run
                .lines()
                .take_while(|line| line.ends_with('\\') || line.contains("--privileged"))
                .collect();

            assert!(
                block.iter().all(|line| !line.trim().is_empty()),
                "blank line inside the docker run for {:?}",
                ports
            );
            assert!(block.last().expect("a last line").contains("--privileged"));
        }
    }

    #[test]
    fn a_cidr_needs_a_prefix_length() {
        assert!(parse_cidr("68.104.236.252").is_err());
    }

    #[test]
    fn a_valid_cidr_round_trips() {
        assert_eq!(parse_cidr("68.104.236.252/32").unwrap(), "68.104.236.252/32");
        assert_eq!(parse_cidr("0.0.0.0/0").unwrap(), "0.0.0.0/0");
        assert_eq!(parse_cidr("10.0.0.0/8").unwrap(), "10.0.0.0/8");
    }

    #[test]
    fn refuses_a_prefix_wider_than_the_address_space() {
        assert!(parse_cidr("10.0.0.0/33").is_err());
    }

    #[test]
    fn refuses_something_that_is_not_an_address() {
        assert!(parse_cidr("my-house/32").is_err());
        assert!(parse_cidr("10.0.0/24").is_err());
        assert!(parse_cidr("999.0.0.1/32").is_err());
    }

    #[test]
    fn user_data_hardens_sshd_and_validates_before_reloading() {
        let rendered = render_sshd_hardening(include_str!("../user-data"));
        let rendered = rendered.as_str();

        assert!(rendered.contains("PermitRootLogin no"));
        assert!(rendered.contains("X11Forwarding no"));
        assert!(rendered.contains("AllowAgentForwarding no"));
        // The validate-then-reload pair is the part that keeps a bad drop-in from locking the
        // instance out, so assert the ordering rather than just the presence of each half.
        assert!(rendered.contains("sudo sshd -t && sudo systemctl reload ssh"));
        // Deliberately unset -- see the comment in user-data. The comment names the directive, so
        // check no *active* line sets it rather than that the word is absent. A future change
        // flipping this on has to update this test, and read why it was left alone.
        assert!(
            !rendered
                .lines()
                .any(|l| l.trim_start().starts_with("AllowTcpForwarding")),
            "AllowTcpForwarding is set as a directive; it is meant to stay unset pending evidence"
        );
        // Applied as a heredoc; an unbalanced marker would break provisioning.
        assert_eq!(rendered.matches("SSHEOF").count(), 2);
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

    #[test]
    fn both_modes_are_born_with_the_same_hardened_sshd() {
        // Docker mode shipped without any hardening at all until the drop-in was pulled out of the
        // Helm template, so assert the two modes agree rather than that each contains the text.
        let helm = render_sshd_hardening(include_str!("../user-data"));
        let docker = render_sshd_hardening(&render_docker_user_data("rancher/rancher", "head", &[]));

        for script in [&helm, &docker] {
            assert!(script.contains("PermitRootLogin no"));
            assert!(script.contains("X11Forwarding no"));
            assert!(script.contains("AllowAgentForwarding no"));
            assert!(script.contains("LogLevel VERBOSE"));
            assert!(script.contains("sudo sshd -t && sudo systemctl reload ssh"));
            // An unsubstituted placeholder would reach the box as a bare quoted string and abort
            // the script under `set -e`.
            assert!(!script.contains("<SSHD_HARDENING>"));
            assert_eq!(script.matches("SSHEOF").count(), 2);
        }
    }

    #[test]
    fn every_template_carries_the_hardening_placeholder() {
        // A new cloud-init template that forgets the placeholder is the failure this guards: it
        // provisions fine and quietly hands out stock sshd.
        assert!(include_str!("../user-data").contains("\"<SSHD_HARDENING>\""));
        assert!(include_str!("../user-data-docker").contains("\"<SSHD_HARDENING>\""));
    }

    #[test]
    fn self_survives_cidr_validation() {
        assert_eq!(parse_cidr("self").unwrap(), "self");
        assert_eq!(parse_cidr("  SELF  ").unwrap(), "self");
    }

    #[tokio::test]
    async fn resolving_a_literal_cidr_asks_nobody() {
        // Only `self` is worth a network round trip; anything else is already an address.
        assert_eq!(resolve_ssh_cidr("68.104.236.252/32").await.unwrap(), "68.104.236.252/32");
        assert_eq!(resolve_ssh_cidr("0.0.0.0/0").await.unwrap(), "0.0.0.0/0");
    }
}
