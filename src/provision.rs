use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;
use aws_sdk_ec2::Client;
use aws_sdk_ec2::types::{BlockDeviceMapping, EbsBlockDevice, Tag, TagSpecification, InstanceNetworkInterfaceSpecification, InstanceType};
use clap::{Parser};
use base64::{engine::general_purpose, Engine};
use crate::network::create_security_group;

#[derive(Parser, Debug)]
pub struct ProvisionArgs {
    #[arg(long = "name")]
    name: String,

    #[arg(long, default_value_t = 64)]
    storage_gb: i32,

    #[arg(long, env = "ROA_VPC_ID")]
    vpc_id: String,

    #[arg(long, env = "ROA_SUBNET_ID")]
    subnet_id: String,

    #[arg(long)]
    key_name: String,

    #[arg(long, env = "ROA_SECURITY_GROUP_ID")]
    security_group_id: Option<String>,

    #[arg(long)]
    email: String,
}

pub async fn provision(args: ProvisionArgs) -> Result<(), Box<dyn std::error::Error>> {
    let region_provider = RegionProviderChain::default_provider().or_else("us-west-2");
    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;
    let client = Client::new(&config);

    let user_data_script = include_str!("../user-data")
        .replace("<LETS_ENCRYPT_EMAIL>", &args.email);
    let user_data = general_purpose::STANDARD.encode(user_data_script);

    let ami_id = "ami-00f46ccd1cbfb363e";

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
        Some(id) => id.into(),
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
        .image_id(ami_id)
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

    Ok(())
}
