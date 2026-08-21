use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use aws_config::meta::region::RegionProviderChain;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_ec2::types::Filter;
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::instance::{Instance, load_instances, manifest_path};
use crate::provision::RancherRepo;

const K3S_CHANNELS_URL: &str = "https://update.k3s.io/v1-release/channels";
const SNAPSHOT_MAX_AGE_DAYS: i64 = 7;
const DISK_ATTENTION_PERCENT: u8 = 80;

#[derive(Parser, Debug)]
pub struct MaintainArgs {
    #[arg(long = "name", help = "Instance name as recorded in the local manifest")]
    name: String,

    #[arg(long, default_value = "ubuntu", help = "SSH user on the instance")]
    ssh_user: String,

    #[arg(
        long,
        env = "ROA_SSH_KEY",
        help = "Private key for SSH. Falls back to ssh-agent and ~/.ssh/config when unset",
        hide_env = true
    )]
    ssh_key: Option<String>,

    #[arg(
        long,
        default_value = "prime-latest",
        help = "Rancher chart repo to compare against. Only used when the manifest does not record one"
    )]
    rancher_repo: RancherRepo,

    #[arg(long, default_value_t = false, help = "Emit the report as JSON")]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Attention,
    Unknown,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Attention => "ATTENTION",
            Status::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: &str, status: Status, detail: impl Into<String>) -> Self {
        // Details land in an aligned table, and ssh in particular is happy to return several
        // lines of warning, so flatten before storing.
        let detail = detail
            .into()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; ");

        Check { name: name.to_string(), status, detail }
    }

    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Check::new(name, Status::Ok, detail)
    }

    fn attention(name: &str, detail: impl Into<String>) -> Self {
        Check::new(name, Status::Attention, detail)
    }

    fn unknown(name: &str, detail: impl Into<String>) -> Self {
        Check::new(name, Status::Unknown, detail)
    }
}

#[derive(Debug, Serialize)]
struct Report {
    name: String,
    instance_id: String,
    fqdn: String,
    region: String,
    checks: Vec<Check>,
}

pub async fn maintain(args: MaintainArgs) -> Result<(), Box<dyn std::error::Error>> {
    let path = manifest_path();
    let instances = load_instances(&path)?;

    let instance = instances
        .iter()
        .find(|i| i.name == args.name)
        .ok_or_else(|| {
            let known: Vec<&str> = instances.iter().map(|i| i.name.as_str()).collect();
            format!(
                "No instance named `{}` in {}. Known instances: {}",
                args.name,
                path.display(),
                if known.is_empty() { "<none>".to_string() } else { known.join(", ") }
            )
        })?
        .clone();

    let repo_url = instance
        .rancher_repo
        .clone()
        .unwrap_or_else(|| args.rancher_repo.value());

    let mut checks = Vec::new();

    // Everything reachable over SSH comes from a single batched command. When it fails, the
    // on-box checks report Unknown and the AWS/HTTP ones still run -- a partial report beats
    // an error.
    let remote = run_remote(&instance, &args);
    let sections = match &remote {
        Ok(output) => split_sections(output),
        Err(e) => {
            checks.push(Check::unknown("ssh", format!("could not reach {}: {}", instance.fqdn, e)));
            BTreeMap::new()
        }
    };

    checks.push(check_os_updates(&sections));
    checks.push(check_reboot_required(&sections));

    let running_k3s = sections.get("k3s_version").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    checks.push(check_k3s_version(running_k3s.as_deref()).await);
    checks.push(check_upgrade_plan(&sections));

    let helm = sections.get("helm");
    let running_chart = helm.and_then(|s| running_rancher_chart(s));
    checks.push(match (helm, &running_chart) {
        (None, _) => Check::unknown("rancher-chart", "not collected"),
        _ => check_rancher_chart(running_chart.as_ref(), &repo_url).await,
    });

    checks.push(check_disk(&sections));
    checks.push(check_clusters(&sections));
    checks.push(check_snapshot(&instance).await);

    let report = Report {
        name: instance.name.clone(),
        instance_id: instance.instance_id.clone(),
        fqdn: instance.fqdn.clone(),
        region: instance.region.clone(),
        checks,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if report.checks.iter().any(|c| c.status == Status::Attention) {
        std::process::exit(1);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// SSH
// ---------------------------------------------------------------------------

// One round trip. Each check writes a `###<name>` marker followed by its output, so a single
// stdout can be split back apart. Failures inside the script are swallowed per-check rather
// than aborting the batch.
const REMOTE_SCRIPT: &str = r#"
export KUBECONFIG=${KUBECONFIG:-$HOME/.kube/config}
echo '###apt'
if [ -x /usr/lib/update-notifier/apt-check ]; then
  # Writes `total;security` to stderr with no trailing newline.
  /usr/lib/update-notifier/apt-check 2>&1
  echo
else
  total=$(apt list --upgradable 2>/dev/null | tail -n +2 | grep -c . || true)
  security=$(apt list --upgradable 2>/dev/null | tail -n +2 | grep -c -- '-security' || true)
  echo "${total};${security}"
fi
echo '###reboot'
if [ -f /var/run/reboot-required ]; then
  echo required
  cat /var/run/reboot-required.pkgs 2>/dev/null | tr '\n' ' '
  echo
else
  echo none
fi
echo '###k3s_version'
kubectl get nodes -o jsonpath='{.items[0].status.nodeInfo.kubeletVersion}' 2>/dev/null
echo
echo '###plans'
kubectl get plans.upgrade.cattle.io -A -o json 2>/dev/null
echo '###helm'
helm list -n cattle-system -o json 2>/dev/null
echo '###disk'
df -P / | tail -n 1
sudo -n du -sh /var/lib/rancher/k3s/agent/containerd 2>/dev/null \
  || du -sh /var/lib/rancher/k3s/agent/containerd 2>/dev/null \
  || echo 'unreadable containerd'
echo '###clusters'
kubectl get clusters.management.cattle.io -o json 2>/dev/null
echo '###end'
"#;

fn run_remote(instance: &Instance, args: &MaintainArgs) -> Result<String, Box<dyn std::error::Error>> {
    let mut cmd = Command::new("ssh");

    // Never block on a prompt: this is meant to be scriptable.
    cmd.arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("ConnectTimeout=10")
        .arg("-o").arg("StrictHostKeyChecking=accept-new");

    if let Some(key) = &args.ssh_key {
        cmd.arg("-i").arg(key);
    }

    // The FQDN, not the manifest's public_ip -- the instance has no Elastic IP, so the
    // recorded address goes stale across a stop/start while DNS stays authoritative.
    cmd.arg(format!("{}@{}", args.ssh_user, instance.fqdn))
        .arg("bash -s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().ok_or("failed to open ssh stdin")?;
        stdin.write_all(REMOTE_SCRIPT.as_bytes())?;
    }

    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // The remote script only exits non-zero if ssh itself failed to connect; per-check
    // failures are already swallowed above.
    if !output.status.success() && !stdout.contains("###end") {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("ssh exited with {}", output.status)
        } else {
            stderr
        }
        .into());
    }

    Ok(stdout)
}

fn split_sections(output: &str) -> BTreeMap<String, String> {
    let mut sections = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut buffer = String::new();

    for line in output.lines() {
        if let Some(name) = line.strip_prefix("###") {
            if let Some(key) = current.take() {
                sections.insert(key, std::mem::take(&mut buffer));
            }
            if name != "end" {
                current = Some(name.to_string());
            }
        } else if current.is_some() {
            buffer.push_str(line);
            buffer.push('\n');
        }
    }

    if let Some(key) = current {
        sections.insert(key, buffer);
    }

    sections
}

// ---------------------------------------------------------------------------
// On-box checks
// ---------------------------------------------------------------------------

fn check_os_updates(sections: &BTreeMap<String, String>) -> Check {
    let Some(raw) = sections.get("apt") else {
        return Check::unknown("os-updates", "not collected");
    };

    // apt-check writes `total;security` to stderr, which the script folds into stdout.
    let line = raw.lines().find(|l| l.contains(';')).unwrap_or("").trim();
    let mut parts = line.split(';');

    match (parts.next().and_then(|p| p.trim().parse::<u32>().ok()),
           parts.next().and_then(|p| p.trim().parse::<u32>().ok())) {
        (Some(total), Some(security)) => {
            let detail = format!("{} pending, {} security", total, security);
            if security > 0 {
                Check::attention("os-updates", detail)
            } else {
                Check::ok("os-updates", detail)
            }
        }
        _ => Check::unknown("os-updates", format!("unparsed apt output: {}", line)),
    }
}

fn check_reboot_required(sections: &BTreeMap<String, String>) -> Check {
    let Some(raw) = sections.get("reboot") else {
        return Check::unknown("reboot", "not collected");
    };

    let mut lines = raw.lines();
    match lines.next().map(str::trim) {
        Some("required") => {
            let pkgs = lines.next().unwrap_or("").trim();
            let detail = if pkgs.is_empty() {
                "reboot required".to_string()
            } else {
                format!("reboot required for: {}", pkgs)
            };
            Check::attention("reboot", detail)
        }
        Some("none") => Check::ok("reboot", "not required"),
        _ => Check::unknown("reboot", "not collected"),
    }
}

#[derive(Deserialize)]
struct K3sChannels {
    data: Vec<K3sChannel>,
}

#[derive(Deserialize)]
struct K3sChannel {
    id: String,
    #[serde(default)]
    latest: Option<String>,
}

// Latest release in the channel for the running version's *minor*. Deliberately not `stable`
// or `latest` -- those cross minors and would push the node past the Rancher chart's
// kubeVersion bound.
async fn channel_latest(minor: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let body = reqwest::Client::new()
        .get(K3S_CHANNELS_URL)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .text()
        .await?;

    let channels: K3sChannels = serde_json::from_str(&body)?;

    Ok(channels
        .data
        .into_iter()
        .find(|c| c.id == minor)
        .and_then(|c| c.latest))
}

// `v1.36.3+k3s1` -> `v1.36`
pub fn minor_of(version: &str) -> Option<String> {
    let stripped = version.trim().trim_start_matches('v');
    let mut parts = stripped.split('.');
    let major = parts.next().filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())?;
    let minor = parts.next().filter(|p| p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty())?;
    Some(format!("v{}.{}", major, minor))
}

async fn check_k3s_version(running: Option<&str>) -> Check {
    let Some(running) = running else {
        return Check::unknown("k3s-version", "not collected");
    };

    let Some(minor) = minor_of(running) else {
        return Check::unknown("k3s-version", format!("running {} (unparsed)", running));
    };

    match channel_latest(&minor).await {
        Ok(Some(latest)) => {
            if latest == running {
                Check::ok("k3s-version", format!("{} (current for channel {})", running, minor))
            } else {
                Check::attention(
                    "k3s-version",
                    format!("running {}, channel {} offers {}", running, minor, latest),
                )
            }
        }
        Ok(None) => Check::unknown("k3s-version", format!("running {}, no channel {} upstream", running, minor)),
        Err(e) => Check::unknown("k3s-version", format!("running {}, channel lookup failed: {}", running, e)),
    }
}

fn check_upgrade_plan(sections: &BTreeMap<String, String>) -> Check {
    let Some(raw) = sections.get("plans") else {
        return Check::unknown("k3s-upgrade-plan", "not collected");
    };

    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Check::unknown("k3s-upgrade-plan", "could not read plans");
    };

    let items = json.get("items").and_then(|i| i.as_array()).map(Vec::as_slice).unwrap_or(&[]);

    if items.is_empty() {
        return Check::attention(
            "k3s-upgrade-plan",
            "no system-upgrade-controller Plan -- k3s patches are not being applied",
        );
    }

    let mut details = Vec::new();
    let mut applying = false;

    for item in items {
        let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
        let channel = item.pointer("/spec/channel").and_then(|v| v.as_str());
        let version = item.pointer("/spec/version").and_then(|v| v.as_str());
        let in_flight = item
            .pointer("/status/applying")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false);

        if in_flight {
            applying = true;
        }

        let target = match (channel, version) {
            (Some(c), _) => c.rsplit('/').next().unwrap_or(c).to_string(),
            (None, Some(v)) => format!("pinned {}", v),
            _ => "no channel or version".to_string(),
        };

        details.push(format!("{} -> {}{}", name, target, if in_flight { " (applying)" } else { "" }));
    }

    let detail = details.join(", ");

    // A Plan mid-apply is not a failure, but it is worth a human's eyes.
    if applying {
        Check::attention("k3s-upgrade-plan", format!("{} -- upgrade in flight", detail))
    } else {
        Check::ok("k3s-upgrade-plan", detail)
    }
}

#[derive(Debug, Clone)]
pub struct RunningChart {
    pub chart: String,
    pub app_version: String,
}

// `helm list -o json` gives `chart: "rancher-2.16.0-<sha>-head"`.
fn running_rancher_chart(raw: &str) -> Option<RunningChart> {
    let releases: Vec<serde_json::Value> = serde_json::from_str(raw.trim()).ok()?;

    releases
        .into_iter()
        .find(|r| r.get("name").and_then(|v| v.as_str()) == Some("rancher"))
        .map(|r| RunningChart {
            chart: r.get("chart").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            app_version: r.get("app_version").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        })
}

// ---------------------------------------------------------------------------
// Chart index
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChartIndex {
    #[serde(default)]
    entries: BTreeMap<String, Vec<ChartEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChartEntry {
    pub version: String,
    #[serde(rename = "appVersion", default)]
    pub app_version: String,
    pub created: DateTime<Utc>,
}

// Newest `<minor>.*-head` chart in the index, chosen by publish date.
//
// NEVER pick by semver. This repo carries 2.14, 2.15 and 2.16 pre-release charts side by side
// and every head version is `<minor>.0-<sha>-head`, so semver ordering ranks them by an
// arbitrary hex string and picks the wrong chart.
pub fn newest_head_chart(index: &str, minor: &str) -> Result<Option<ChartEntry>, Box<dyn std::error::Error>> {
    let index: ChartIndex = serde_norway::from_str(index)?;

    let Some(entries) = index.entries.get("rancher") else {
        return Ok(None);
    };

    let prefix = format!("{}.", minor.trim_start_matches('v'));

    Ok(entries
        .iter()
        .filter(|e| e.version.starts_with(&prefix) && e.version.ends_with("-head"))
        .max_by_key(|e| e.created)
        .cloned())
}

// `rancher-2.16.0-<sha>-head` -> `2.16`, and `rancher-2.16-<sha>-head` -> `2.16`.
//
// The minor is truncated at the first non-digit because the pre-release repos publish both
// shapes: most head charts are `<major>.<minor>.<patch>-<sha>-head`, but some carry no patch
// component at all and the SHA runs straight into the minor. Splitting on `.` alone hands back
// the whole SHA-bearing string, which then matches no chart in the index.
fn chart_minor(chart: &str) -> Option<String> {
    let version = chart.strip_prefix("rancher-")?;
    let mut parts = version.split('.');

    let major = parts
        .next()
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))?;

    let minor: String = parts.next()?.chars().take_while(char::is_ascii_digit).collect();
    if minor.is_empty() {
        return None;
    }

    Some(format!("{}.{}", major, minor))
}

async fn check_rancher_chart(running: Option<&RunningChart>, repo_url: &str) -> Check {
    let Some(running) = running else {
        return Check::unknown("rancher-chart", "no `rancher` release found in cattle-system");
    };

    let Some(minor) = chart_minor(&running.chart) else {
        return Check::unknown("rancher-chart", format!("running {} (unparsed)", running.chart));
    };

    let url = format!("{}/index.yaml", repo_url.trim_end_matches('/'));

    let body = match reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(60))
        .send()
        .await
    {
        Ok(resp) => match resp.text().await {
            Ok(body) => body,
            Err(e) => return Check::unknown("rancher-chart", format!("running {}, index read failed: {}", running.chart, e)),
        },
        Err(e) => return Check::unknown("rancher-chart", format!("running {}, index fetch failed: {}", running.chart, e)),
    };

    let newest = match newest_head_chart(&body, &minor) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return Check::unknown(
                "rancher-chart",
                format!("running {}, no {}.*-head charts in {}", running.chart, minor, url),
            );
        }
        Err(e) => return Check::unknown("rancher-chart", format!("running {}, index parse failed: {}", running.chart, e)),
    };

    let running_version = running.chart.strip_prefix("rancher-").unwrap_or(&running.chart);

    if running_version == newest.version {
        Check::ok(
            "rancher-chart",
            format!("{} is the newest head ({})", running.chart, newest.created.format("%Y-%m-%d")),
        )
    } else {
        Check::attention(
            "rancher-chart",
            format!(
                "running {} (app {}); newest head is {} (app {}) published {}",
                running.chart,
                running.app_version,
                newest.version,
                newest.app_version,
                newest.created.format("%Y-%m-%d %H:%M UTC"),
            ),
        )
    }
}

// ---------------------------------------------------------------------------
// Disk and clusters
// ---------------------------------------------------------------------------

fn check_disk(sections: &BTreeMap<String, String>) -> Check {
    let Some(raw) = sections.get("disk") else {
        return Check::unknown("disk", "not collected");
    };

    let mut lines = raw.lines();
    let Some(df) = lines.next() else {
        return Check::unknown("disk", "not collected");
    };

    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    let fields: Vec<&str> = df.split_whitespace().collect();
    let Some(percent) = fields
        .get(4)
        .and_then(|p| p.trim_end_matches('%').parse::<u8>().ok())
    else {
        return Check::unknown("disk", format!("unparsed df output: {}", df.trim()));
    };

    let containerd = lines
        .next()
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("unknown");

    let detail = format!("root {}% used, containerd {}", percent, containerd);

    if percent >= DISK_ATTENTION_PERCENT {
        Check::attention("disk", detail)
    } else {
        Check::ok("disk", detail)
    }
}

fn check_clusters(sections: &BTreeMap<String, String>) -> Check {
    let Some(raw) = sections.get("clusters") else {
        return Check::unknown("clusters", "not collected");
    };

    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Check::unknown("clusters", "could not read clusters");
    };

    let items = json.get("items").and_then(|i| i.as_array()).map(Vec::as_slice).unwrap_or(&[]);

    if items.is_empty() {
        return Check::unknown("clusters", "no management clusters returned");
    }

    let mut details = Vec::new();
    let mut degraded = false;

    for item in items {
        let id = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
        let display = item
            .pointer("/spec/displayName")
            .and_then(|v| v.as_str())
            .unwrap_or(id);

        let conditions = item.pointer("/status/conditions").and_then(|v| v.as_array());
        let condition = |name: &str| -> Option<String> {
            conditions?
                .iter()
                .find(|c| c.get("type").and_then(|v| v.as_str()) == Some(name))
                .and_then(|c| c.get("status").and_then(|v| v.as_str()))
                .map(str::to_string)
        };

        let ready = condition("Ready");
        let connected = condition("Connected");
        let agent_deployed = condition("AgentDeployed");

        let is_local = id == "local";

        // `local` has no Connected condition; downstreams do.
        let healthy = if is_local {
            ready.as_deref() == Some("True")
        } else {
            connected.as_deref() == Some("True")
        };

        // Connected=False alongside AgentDeployed=True is normal mid-registration, so it is
        // reported as such rather than as a hard failure.
        let registering = !is_local
            && connected.as_deref() == Some("False")
            && agent_deployed.as_deref() == Some("True");

        if !healthy && !registering {
            degraded = true;
        }

        let state = if is_local {
            format!("Ready={}", ready.as_deref().unwrap_or("?"))
        } else if registering {
            "registering".to_string()
        } else {
            format!("Connected={}", connected.as_deref().unwrap_or("?"))
        };

        details.push(format!("{} ({}) {}", display, id, state));
    }

    let detail = details.join(", ");

    if degraded {
        Check::attention("clusters", detail)
    } else {
        Check::ok("clusters", detail)
    }
}

// ---------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------

async fn check_snapshot(instance: &Instance) -> Check {
    match newest_snapshot(instance).await {
        Ok(Some((id, started, state))) => {
            let age = Utc::now().signed_duration_since(started).num_days();
            let detail = format!(
                "{} ({}) {} days old, {}",
                id,
                started.format("%Y-%m-%d"),
                age,
                state
            );

            if age > SNAPSHOT_MAX_AGE_DAYS || state != "completed" {
                Check::attention("snapshot", detail)
            } else {
                Check::ok("snapshot", detail)
            }
        }
        Ok(None) => Check::attention("snapshot", "no snapshots of the root volume -- no restore point"),
        Err(e) => Check::unknown("snapshot", format!("lookup failed: {}", e)),
    }
}

async fn newest_snapshot(
    instance: &Instance,
) -> Result<Option<(String, DateTime<Utc>, String)>, Box<dyn std::error::Error>> {
    // Seed the region from the manifest so the report always targets the instance's region
    // rather than whatever the ambient AWS config happens to say.
    let region_provider =
        RegionProviderChain::default_provider().or_else(Region::new(instance.region.clone()));

    let config = aws_config::defaults(BehaviorVersion::latest())
        .region(region_provider)
        .load()
        .await;

    let ec2 = Ec2Client::new(&config);

    let described = ec2
        .describe_instances()
        .instance_ids(&instance.instance_id)
        .send()
        .await?;

    let root_device = described
        .reservations()
        .first()
        .and_then(|r| r.instances().first())
        .ok_or_else(|| format!("instance {} not found", instance.instance_id))?;

    let root_name = root_device.root_device_name().unwrap_or("/dev/sda1");

    let volume_id = root_device
        .block_device_mappings()
        .iter()
        .find(|m| m.device_name() == Some(root_name))
        .and_then(|m| m.ebs())
        .and_then(|e| e.volume_id())
        .ok_or("could not resolve the root volume")?;

    let snapshots = ec2
        .describe_snapshots()
        .owner_ids("self")
        .filters(Filter::builder().name("volume-id").values(volume_id).build())
        .send()
        .await?;

    Ok(snapshots
        .snapshots()
        .iter()
        .filter_map(|s| {
            let id = s.snapshot_id()?.to_string();
            let started = s.start_time()?;
            let started = DateTime::from_timestamp(started.secs(), 0)?;
            let state = s.state().map(|st| st.as_str().to_string()).unwrap_or_else(|| "unknown".into());
            Some((id, started, state))
        })
        .max_by_key(|(_, started, _)| *started))
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_report(report: &Report) {
    println!(
        "{} ({}) {} [{}]",
        report.name, report.instance_id, report.fqdn, report.region
    );
    println!();

    let width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);

    for check in &report.checks {
        println!(
            "  {:<width$}  {:<9}  {}",
            check.name,
            check.status.label(),
            check.detail,
            width = width
        );
    }

    let attention = report.checks.iter().filter(|c| c.status == Status::Attention).count();
    let unknown = report.checks.iter().filter(|c| c.status == Status::Unknown).count();

    println!();
    println!("{} need attention, {} unknown", attention, unknown);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deliberately out of publish order, and with a `-head` version whose hex suffix sorts
    // *after* the newest one under any lexical or semver ordering.
    const INDEX: &str = r#"
apiVersion: v1
entries:
  rancher:
    - appVersion: v2.16.0-ffffffff-head
      created: "2026-08-14T15:42:29.693711677Z"
      version: 2.16.0-ffffffff-head
    - appVersion: v2.16.0-0000aaaa-head
      created: "2026-08-18T15:38:04.751911157Z"
      version: 2.16.0-0000aaaa-head
    - appVersion: v2.15.4-bbbbbbbb-head
      created: "2026-08-19T09:00:00.000000000Z"
      version: 2.15.4-bbbbbbbb-head
    - appVersion: v2.16.0
      created: "2026-08-20T09:00:00.000000000Z"
      version: 2.16.0
"#;

    #[test]
    fn picks_the_newest_head_chart_by_publish_date_not_semver() {
        let newest = newest_head_chart(INDEX, "2.16").unwrap().expect("a 2.16 head chart");

        // `ffffffff` beats `0000aaaa` lexically and under semver's pre-release rules; only the
        // publish date gets this right.
        assert_eq!(newest.version, "2.16.0-0000aaaa-head");
        assert_eq!(newest.app_version, "v2.16.0-0000aaaa-head");
    }

    #[test]
    fn ignores_other_minors_and_non_head_charts() {
        let newest = newest_head_chart(INDEX, "2.16").unwrap().unwrap();

        // The 2.15 head is newer, and so is the 2.16.0 GA chart -- neither is a candidate.
        assert!(!newest.version.starts_with("2.15"));
        assert!(newest.version.ends_with("-head"));

        let older_line = newest_head_chart(INDEX, "2.15").unwrap().unwrap();
        assert_eq!(older_line.version, "2.15.4-bbbbbbbb-head");

        assert!(newest_head_chart(INDEX, "2.14").unwrap().is_none());
    }

    #[test]
    fn tolerates_a_v_prefixed_minor() {
        assert_eq!(
            newest_head_chart(INDEX, "v2.16").unwrap().unwrap().version,
            "2.16.0-0000aaaa-head"
        );
    }

    #[test]
    fn reads_the_minor_off_a_running_helm_chart() {
        assert_eq!(chart_minor("rancher-2.16.0-9575c72-head").as_deref(), Some("2.16"));
        assert_eq!(chart_minor("rancher-2.14.3").as_deref(), Some("2.14"));
        assert_eq!(chart_minor("cert-manager-v1.16.3"), None);

        // Published without a patch component -- the SHA runs straight into the minor.
        assert_eq!(
            chart_minor("rancher-2.16-ffbd1cc914559930ba6aaa03bc434ef32aefe490-head").as_deref(),
            Some("2.16")
        );

        // Nothing numeric to read is not a minor.
        assert_eq!(chart_minor("rancher-devel"), None);
        assert_eq!(chart_minor("rancher-v2.16.0"), None);
    }

    #[test]
    fn reads_the_minor_off_a_k3s_version() {
        assert_eq!(minor_of("v1.36.3+k3s1").as_deref(), Some("v1.36"));
        assert_eq!(minor_of("v1.36.3").as_deref(), Some("v1.36"));
        assert_eq!(minor_of("stable"), None);
    }

    #[test]
    fn parses_the_k3s_channels_payload() {
        // Trimmed from the real https://update.k3s.io/v1-release/channels response.
        let body = r#"{"type":"collection","data":[
            {"id":"stable","type":"channel","name":"stable","latest":"v1.36.3+k3s1"},
            {"id":"testing","type":"channel","name":"testing"},
            {"id":"v1.36","type":"channel","name":"v1.36","latest":"v1.36.3+k3s1"}
        ]}"#;

        let channels: K3sChannels = serde_json::from_str(body).unwrap();
        let v136 = channels.data.iter().find(|c| c.id == "v1.36").unwrap();
        assert_eq!(v136.latest.as_deref(), Some("v1.36.3+k3s1"));

        // A channel with no release yet must not blow up deserialization.
        let testing = channels.data.iter().find(|c| c.id == "testing").unwrap();
        assert_eq!(testing.latest, None);
    }

    #[test]
    fn splits_the_batched_remote_output_into_sections() {
        let output = "###apt\n191;137\n###reboot\nnone\n###k3s_version\nv1.36.3+k3s1\n\n###end\n";
        let sections = split_sections(output);

        assert_eq!(sections.get("apt").unwrap().trim(), "191;137");
        assert_eq!(sections.get("reboot").unwrap().trim(), "none");
        assert_eq!(sections.get("k3s_version").unwrap().trim(), "v1.36.3+k3s1");
        assert!(!sections.contains_key("end"));
    }

    #[test]
    fn flags_pending_security_updates() {
        let mut sections = BTreeMap::new();
        sections.insert("apt".to_string(), "191;137\n".to_string());
        let check = check_os_updates(&sections);
        assert_eq!(check.status, Status::Attention);
        assert!(check.detail.contains("137 security"));

        sections.insert("apt".to_string(), "3;0\n".to_string());
        assert_eq!(check_os_updates(&sections).status, Status::Ok);
    }

    #[test]
    fn flags_a_missing_upgrade_plan() {
        let mut sections = BTreeMap::new();
        sections.insert("plans".to_string(), r#"{"items":[]}"#.to_string());

        let check = check_upgrade_plan(&sections);
        assert_eq!(check.status, Status::Attention);
        assert!(check.detail.contains("no system-upgrade-controller Plan"));
    }

    #[test]
    fn reports_a_healthy_channel_pinned_plan() {
        let mut sections = BTreeMap::new();
        sections.insert(
            "plans".to_string(),
            r#"{"items":[{"metadata":{"name":"k3s-server"},
                          "spec":{"channel":"https://update.k3s.io/v1-release/channels/v1.36"},
                          "status":{"applying":[]}}]}"#
                .to_string(),
        );

        let check = check_upgrade_plan(&sections);
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("k3s-server -> v1.36"));
    }

    #[test]
    fn treats_a_registering_downstream_as_healthy_but_a_disconnected_one_as_not() {
        let registering = r#"{"items":[
            {"metadata":{"name":"local"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
            {"metadata":{"name":"c-m-5g5m2dxf"},"spec":{"displayName":"mo-test"},
             "status":{"conditions":[{"type":"Connected","status":"False"},
                                     {"type":"AgentDeployed","status":"True"}]}}]}"#;

        let mut sections = BTreeMap::new();
        sections.insert("clusters".to_string(), registering.to_string());
        let check = check_clusters(&sections);
        assert_eq!(check.status, Status::Ok);
        assert!(check.detail.contains("mo-test (c-m-5g5m2dxf) registering"));

        let disconnected = r#"{"items":[
            {"metadata":{"name":"local"},"status":{"conditions":[{"type":"Ready","status":"True"}]}},
            {"metadata":{"name":"c-m-5g5m2dxf"},"spec":{"displayName":"mo-test"},
             "status":{"conditions":[{"type":"Connected","status":"False"},
                                     {"type":"AgentDeployed","status":"False"}]}}]}"#;

        sections.insert("clusters".to_string(), disconnected.to_string());
        assert_eq!(check_clusters(&sections).status, Status::Attention);
    }

    #[test]
    fn flags_a_full_root_disk() {
        let mut sections = BTreeMap::new();
        sections.insert(
            "disk".to_string(),
            "/dev/root  64891708 30112232  34762668  47% /\n19G\t/var/lib/rancher/k3s/agent/containerd\n"
                .to_string(),
        );

        let check = check_disk(&sections);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(check.detail, "root 47% used, containerd 19G");

        sections.insert(
            "disk".to_string(),
            "/dev/root  64891708 60112232  4762668  93% /\n55G\t/var/lib/rancher/k3s/agent/containerd\n"
                .to_string(),
        );
        assert_eq!(check_disk(&sections).status, Status::Attention);
    }

    #[test]
    fn reads_the_running_rancher_release_out_of_helm_list() {
        let raw = r#"[{"name":"cert-manager","chart":"cert-manager-v1.16.3","app_version":"v1.16.3"},
                      {"name":"rancher","chart":"rancher-2.16.0-9575c72-head","app_version":"v2.16.0-9575c72-head"}]"#;

        let running = running_rancher_chart(raw).expect("a rancher release");
        assert_eq!(running.chart, "rancher-2.16.0-9575c72-head");
        assert_eq!(running.app_version, "v2.16.0-9575c72-head");

        assert!(running_rancher_chart("[]").is_none());
        assert!(running_rancher_chart("not json").is_none());
    }
}
