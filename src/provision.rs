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
enum RancherRepo {
    Latest,
    Prime,
    Alpha,
    ReleaseLine(String),
}

impl std::str::FromStr for RancherRepo {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "latest" => Ok(RancherRepo::Latest),
            "prime" => Ok(RancherRepo::Prime),
            "alpha" => Ok(RancherRepo::Alpha),
            _ if s.starts_with("release-") => Ok(RancherRepo::ReleaseLine(s.to_string())),
            _ => Err(format!("Invalid rancher repo: {}", s)),
        }
    }
}

impl RancherRepo {
    fn value(&self) -> String {
        match &self {
            RancherRepo::Latest => "https://releases.rancher.com/server-charts/latest".to_string(),
            RancherRepo::Prime => "https://charts.rancher.com/server-charts/prime".to_string(),
            RancherRepo::Alpha => "https://charts.optimus.rancher.io/server-charts/alpha".to_string(),
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

    #[arg(long, value_enum, default_value_t = ProvisionMode::Helm, help = "Install method: `helm` (k3d + Helm) or `docker`")]
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

    #[arg(long, value_enum, help = "Rancher Helm chart repo: `latest`, `prime`, `alpha`, or release-<major>-<minor>")]
    rancher_repo: RancherRepo,

    #[arg(long, help = "Pin a specific Rancher version (e.g. `v2.14.0`)")]
    rancher_version: Option<String>,

    #[arg(long, default_value = "rancher/rancher", help = "Docker image registry (Docker mode only)")]
    docker_registry: String,

    #[arg(long, help = "Override the Rancher hostname")]
    rancher_hostname: Option<String>,

    #[arg(long, alias="password", help = "Set the Rancher bootstrap password")]
    rancher_bootstrap_password: Option<String>,

    #[arg(long, help = "Pin a specific k3s version for the k3d cluster (e.g. `v1.33.1-k3s1`). Required when Rancher's kubeVersion constraint excludes the latest k3s.")]
    k3s_version: Option<String>,

    #[arg(long, env = "ROA_AMI_ID", help = "AMI ID to use (Ubuntu-based recommended)", hide_env = true)]
    ami_id: String,

    #[arg(long, default_value_t = false, help = "Block until DNS propagates and Rancher is reachable")]
    wait_for_ready: bool,
}

// Maps Rancher minor version to the highest k3s version certified by Rancher's support matrix.
// Source: https://www.suse.com/suse-rancher/support-matrix/
fn default_k3s_version(rancher_version: &str) -> Option<&'static str> {
    let stripped = rancher_version.trim_start_matches('v');
    let minor = stripped.splitn(3, '.').take(2).collect::<Vec<_>>().join(".");
    match minor.as_str() {
        "2.11" => Some("v1.32.3-k3s1"),
        "2.12" => Some("v1.33.3-k3s1"),
        "2.13" => Some("v1.34.3-k3s1"),
        "2.14" => Some("v1.35.5-k3s1"),
        _ => None,
    }
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

    let k3s_image_flag = match args.k3s_version.as_deref()
        .or_else(|| args.rancher_version.as_deref().and_then(default_k3s_version))
    {
        Some(version) => {
            println!("Using k3s version: {}", version);
            format!("--image rancher/k3s:{}", version)
        }
        None => String::new(),
    };

    let user_data_script = match args.mode {
        ProvisionMode::Helm => &*include_str!("../user-data")
            .replace("\"<RANCHER_HOSTNAME>\"", &args.rancher_hostname.unwrap_or(fqdn.clone()))
            .replace("\"<LETS_ENCRYPT_EMAIL>\"", &args.email)
            .replace("\"<RANCHER_REPO>\"", rancher_repo)
            .replace("\"<RANCHER_VERSION>\"", &rancher_version)
            .replace("\"<RANCHER_BOOTSTRAP_PASSWORD>\"", bootstrap_password_flag.as_str())
            .replace("\"<K3S_IMAGE>\"", &k3s_image_flag),
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
