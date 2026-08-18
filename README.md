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
| `--mode` | `helm` | Install method: `helm` (k3s + Helm) or `docker` |
| `--storage-gb` | `64` | EBS root volume size in GB |
| `--security-group-id` | *(auto-created)* | Use an existing security group instead of creating one |
| `--rancher-repo` | `latest` | Rancher Helm chart repo: `latest`, `prime`, `prime-latest` (Prime RC and head), `prime-alpha` (alias `alpha`), `community-alpha`, or `release-<major>-<minor>` |
| `--rancher-version` | *(latest dev)* | Pin a specific Rancher version (e.g. `2.9.0`) |
| `--k3s-version` | *(per Rancher minor, else latest stable)* | Pin the k3s version (`INSTALL_K3S_VERSION` form, e.g. `v1.36.2+k3s1`). Pass explicitly with `--rancher-repo alpha`/unpinned Rancher versions, where the default isn't resolved. |
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

### `maintain` — Weekly health report

Reports on an instance's OS patch level, k3s version, `system-upgrade-controller` Plan, running
Rancher chart, disk usage, cluster health and EBS snapshot age. Read-only — it changes nothing.

```
roa maintain --name <NAME> [OPTIONS]
```

| Flag | Env var | Default | Description |
|------|---------|---------|-------------|
| `--name` | | | Instance name as recorded in the local manifest |
| `--ssh-user` | | `ubuntu` | SSH user on the instance |
| `--ssh-key` | `ROA_SSH_KEY` | *(ssh-agent / `~/.ssh/config`)* | Private key for SSH |
| `--rancher-repo` | | `prime-latest` | Chart repo to compare against. Only used when the manifest doesn't record one |
| `--json` | | `false` | Emit the report as JSON |

On-box checks run over SSH in a single batched command; the chart index, k3s release channel and
EBS snapshot checks run locally. If SSH fails the on-box checks report `UNKNOWN` and the rest of
the report still runs.

**Exits non-zero when any check needs attention**, so it can be dropped into a scheduler unchanged.

**Example:**

```bash
roa maintain --name shared
```

```
shared (i-0123456789abcdef0) shared.ui.rancher.space [us-west-2]

  os-updates        ATTENTION  57 pending, 1 security
  reboot            ATTENTION  reboot required for: libc6 linux-image-7.0.0-1010-aws linux-base
  k3s-version       OK         v1.36.3+k3s1 (current for channel v1.36)
  k3s-upgrade-plan  ATTENTION  no system-upgrade-controller Plan -- k3s patches are not being applied
  rancher-chart     ATTENTION  running rancher-2.16.0-9575c72...-head; newest head is 2.16.0-3ad9f7f...-head published 2026-08-18 20:49 UTC
  disk              OK         root 38% used, containerd 20G
  clusters          OK         mo-test (c-m-5g5m2dxf) Connected=True, local (local) Ready=True
  snapshot          ATTENTION  no snapshots of the root volume -- no restore point

5 need attention, 0 unknown
```

Head charts are picked by **publish date**, never by semver — the pre-release repos carry several
Rancher minors side by side and every head version is `<minor>.0-<sha>-head`, which semver ranks by
an arbitrary hex string.

## How it works

1. **Provision** launches a `t3.2xlarge` EC2 instance with a user-data bootstrap script
   - **Helm mode** (default): installs single-node k3s, kubectl, Helm, the system-upgrade-controller, cert-manager, and Rancher via Helm
   - **Docker mode**: runs Rancher directly as a Docker container
2. A security group with the necessary inbound rules is created (or an existing one is reused)
3. An Elastic IP or public IP is assigned, and an A record is upserted in Route 53

## Upgrading k3s

Helm-mode instances run k3s directly on the host, so the management cluster's Kubernetes version is upgradeable. Two options:

### Re-run the installer

Over SSH on the instance:

```bash
curl -sfL https://get.k3s.io | INSTALL_K3S_VERSION=v1.37.1+k3s1 sh -
```

k3s replaces its binary and restarts in place.

### system-upgrade-controller

`system-upgrade-controller` is installed when RoA provisions Rancher, and `provision` now also writes
a Plan tracking the k3s release channel for the **minor** it installed:

```yaml
apiVersion: upgrade.cattle.io/v1
kind: Plan
metadata:
  name: k3s-server
  namespace: system-upgrade
spec:
  concurrency: 1
  channel: https://update.k3s.io/v1-release/channels/v1.36 # minor-pinned, never stable/latest
  serviceAccountName: system-upgrade
  cordon: true
  nodeSelector:
    matchExpressions:
      - { key: node-role.kubernetes.io/control-plane, operator: In, values: ["true"] }
  upgrade:
    image: rancher/k3s-upgrade
```

This keeps k3s current on **patches** without ever crossing a minor. That distinction matters:
Rancher's chart declares an upper Kubernetes bound (the 2.16 head charts use
`kubeVersion: < 1.37.0-0`). The `stable` and `latest` channels sit inside that bound today but will
move to the next minor, and if the controller took the node across it the running Rancher would keep
working while the **next `helm upgrade` was silently refused** — blocking every future Rancher bump.
A `vX.Y` channel tracks patches within the minor and cannot cross it.

No Plan is written when the k3s version is unpinned (`--rancher-repo alpha`/`--devel` without
`--k3s-version`), since there is no minor to pin to.

Moving to a new minor is a deliberate act: bump `default_k3s_version` for the Rancher minor, and
re-point or replace the Plan's channel. Upgrade Rancher itself separately via `helm upgrade` on the
`rancher` release.

## AWS permissions

The IAM principal running `roa` needs at minimum:

- `ec2:RunInstances`, `ec2:DescribeInstances`, `ec2:TerminateInstances`
- `ec2:CreateSecurityGroup`, `ec2:DeleteSecurityGroup`, `ec2:DescribeSecurityGroups`, `ec2:AuthorizeSecurityGroupIngress`
- `route53:ChangeResourceRecordSets`, `route53:GetChange`

`maintain` additionally needs:

- `ec2:DescribeSnapshots`

## Local state

Instance metadata is stored at `~/.config/roa/instances.json`. This file is managed automatically by `provision` and `terminate`.

## License

See [LICENSE](LICENSE).
