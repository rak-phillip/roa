# roa — Rancher on AWS

A CLI tool for provisioning and managing [Rancher](https://rancher.com/) instances on AWS EC2. `roa` handles EC2 instance launch, security group creation, DNS record management via Route 53, and local instance tracking.

## Prerequisites

- An AWS account with permissions for EC2, Route 53, and related services
- AWS credentials configured (via environment variables, `~/.aws/credentials`, or an IAM instance profile)
- An EC2 key pair

## Installation

### Download a pre-built binary

Grab the latest release for your platform from the [Releases](../../releases) page:

| Platform | File            |
|----------|-----------------|
| Linux    | `roa-linux`     |
| macOS    | `roa-macos`     |
| Windows  | `roa-windows.exe` |

Make the binary executable and move it onto your `PATH`:

```bash
chmod +x roa-linux
sudo mv roa-linux /usr/local/bin/roa
```

### Build from source

Requires Rust (toolchain version `1.91.0`:

```bash
git clone https://github.com/<your-org>/roa.git
cd roa
cargo build --release
# Binary is at target/release/roa
```

## Configuration

`roa` reads configuration from environment variables. You can set these in a config file at `~/.config/roa/roa_variables` (loaded automatically on startup), or export them in your shell.

The config file uses `.env` syntax:

```bash
# ~/.config/roa/roa_variables

AWS_REGION=us-west-2

ROA_VPC_ID=vpc-0123456789abcdef0
ROA_SUBNET_ID=subnet-0123456789abcdef0
ROA_HOSTED_ZONE_ID=Z0123456789ABCDEFGHIJ
ROA_AMI_ID=ami-0123456789abcdef0
```

All variables can also be passed as CLI flags (flags take precedence over environment variables).

## Usage

```
roa <COMMAND>
```

### `provision` — Launch a Rancher instance

Launches an EC2 instance, creates a security group, and registers a DNS A record.

```
roa provision --name <NAME> --key-name <KEY_NAME> --email <EMAIL> [OPTIONS]
```

**Required flags:**

| Flag | Env var | Description |
|------|---------|-------------|
| `--name` | | Instance name. Also used as the subdomain: `<name>.ui.rancher.space` |
| `--key-name` | | EC2 key pair name for SSH access |
| `--email` | | Email address for Let's Encrypt certificate issuance |
| `--vpc-id` | `ROA_VPC_ID` | VPC to launch the instance into |
| `--subnet-id` | `ROA_SUBNET_ID` | Subnet to attach the instance to |
| `--hosted-zone-id` | `ROA_HOSTED_ZONE_ID` | Route 53 hosted zone ID for DNS management |
| `--ami-id` | `ROA_AMI_ID` | AMI ID to use (Ubuntu-based recommended) |

**Optional flags:**

| Flag | Default | Description |
|------|---------|-------------|
| `--mode` | `helm` | Install method: `helm` (k3d + Helm) or `docker` |
| `--storage-gb` | `64` | EBS root volume size in GB |
| `--security-group-id` | *(auto-created)* | Use an existing security group instead of creating one |
| `--rancher-repo` | `latest` | Rancher Helm chart repo: `latest`, `prime`, or `alpha` |
| `--rancher-version` | *(latest dev)* | Pin a specific Rancher version (e.g. `2.9.0`) |
| `--rancher-hostname` | `<name>.ui.rancher.space` | Override the Rancher hostname |
| `--docker-registry` | `rancher/rancher` | Docker image registry (Docker mode only) |
| `--wait-for-ready` | `false` | Block until DNS propagates and Rancher is reachable |

**Example:**

```bash
roa provision \
  --name my-rancher \
  --key-name my-keypair \
  --email admin@example.com \
  --wait-for-ready
```

### `terminate` — Terminate a Rancher instance

Terminates the EC2 instance and cleans up the Route 53 DNS record and the security group.

```
roa terminate --instance-id <INSTANCE_ID> [OPTIONS]
```

**Required flags:**

| Flag | Env var | Description |
|------|---------|-------------|
| `--instance-id` | | EC2 instance ID to terminate |
| `--hosted-zone-id` | `ROA_HOSTED_ZONE_ID` | Route 53 hosted zone containing the DNS record |
| `--vpc-id` | `ROA_VPC_ID` | VPC used to locate the security group for deletion |

**Example:**

```bash
roa terminate --instance-id i-0123456789abcdef0
```

### `list` — List provisioned instances

Displays all instances recorded in the local manifest (`~/.config/roa/instances.json`).

```
roa list
```

**Output columns:** `instance_id  name  public_ip  fqdn`

**Example output:**

```
i-0123456789abcdef0  my-rancher  203.0.113.42  my-rancher.ui.rancher.space
```

## How it works

1. **Provision** launches a `t3.2xlarge` EC2 instance with a user-data bootstrap script
   - **Helm mode** (default): installs Docker, k3d, kubectl, Helm, cert-manager, and Rancher via Helm
   - **Docker mode**: runs Rancher directly as a Docker container
2. A security group with the necessary inbound rules is created (or an existing one is reused)
3. An Elastic IP or public IP is assigned, and an A record is upserted in Route 53

## AWS permissions

The IAM principal running `roa` needs at minimum:

- `ec2:RunInstances`, `ec2:DescribeInstances`, `ec2:TerminateInstances`
- `ec2:CreateSecurityGroup`, `ec2:DeleteSecurityGroup`, `ec2:DescribeSecurityGroups`, `ec2:AuthorizeSecurityGroupIngress`
- `route53:ChangeResourceRecordSets`, `route53:GetChange`

## Local state

Instance metadata is stored at `~/.config/roa/instances.json`. This file is managed automatically by `provision` and `terminate`.

## License

See [LICENSE](LICENSE).
