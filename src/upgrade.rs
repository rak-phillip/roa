use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use clap::{ArgGroup, Parser};

use crate::aws::{create_snapshot, ec2_client, root_volume_id, wait_for_snapshot};
use crate::instance::{Instance, load_instances, manifest_path, save_instances};
use crate::maintain::{ChartEntry, RunningChart, chart_minor, newest_head_chart};
use crate::provision::RancherRepo;
use crate::ssh::{SshTarget, run_script, split_sections};

/// Root usage at or above this refuses to start. A head upgrade pulls a fresh image set before it
/// releases the old one, so starting near the line is how the box gets wedged with no disk.
const DISK_REFUSAL_PERCENT: u8 = 85;

/// Charts younger than this are warned about: the chart lands in the index before its images
/// finish pushing to the registry, and the result is ImagePullBackOff rather than a clean failure.
const YOUNG_CHART_MINUTES: i64 = 30;

#[derive(Parser, Debug)]
#[command(group(
    ArgGroup::new("chart_target")
        .required(true)
        .args(["rancher_version", "latest_head"])
))]
pub struct UpgradeArgs {
    #[arg(long = "name", help = "Instance name as recorded in the local manifest")]
    pub name: String,

    #[arg(long, help = "Upgrade to this exact chart version")]
    pub rancher_version: Option<String>,

    #[arg(long, default_value_t = false, help = "Upgrade to the newest head chart in the running minor")]
    pub latest_head: bool,

    // Deliberately not `default_value = "prime-latest"`. A defaulted flag cannot be told apart from
    // one the user typed, and for a command that restarts Rancher the manifest's recorded repo must
    // win over a default nobody chose. Resolution is flag -> manifest -> prime-latest.
    #[arg(long, help = "Chart repo to upgrade from. Defaults to the repo recorded on the instance")]
    pub rancher_repo: Option<RancherRepo>,

    #[arg(long, default_value = "ubuntu", help = "SSH user on the instance")]
    pub ssh_user: String,

    #[arg(
        long,
        env = "ROA_SSH_KEY",
        help = "Private key for SSH. Falls back to ssh-agent and ~/.ssh/config when unset",
        hide_env = true
    )]
    pub ssh_key: Option<String>,

    #[arg(long, default_value_t = false, help = "Select and report, but change nothing")]
    pub dry_run: bool,

    #[arg(long, default_value_t = false, help = "Skip the confirmation prompt")]
    pub yes: bool,

    #[arg(long, default_value_t = false, help = "Do not take a pre-upgrade snapshot. Unsafe")]
    pub skip_snapshot: bool,

    #[arg(long, default_value_t = 90, help = "How long to wait for the snapshot to complete")]
    pub snapshot_timeout_minutes: u64,

    #[arg(long, default_value = "20m", help = "Timeout passed to helm upgrade and rollout status")]
    pub helm_timeout: String,

    #[arg(long, default_value_t = 10, help = "How long to let downstream agents re-register before failing")]
    pub settle_minutes: u64,

    #[arg(long, default_value_t = false, help = "Emit the report as JSON")]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// The shell boundary
// ---------------------------------------------------------------------------

// `--rancher-version` is operator input that ends up inside a bash script piped to a shell on a
// box other people share. These two are security code, not tidiness. Values are single-quoted at
// the interpolation site as well; this is the belt to that pair of braces.

pub fn validate_chart_version(version: &str) -> Result<&str, String> {
    if version.is_empty() {
        return Err("chart version is empty".to_string());
    }
    if version.len() > 128 {
        return Err("chart version is implausibly long".to_string());
    }
    // A leading `-` would be read by helm as a flag rather than a value.
    if version.starts_with('-') {
        return Err(format!("chart version `{}` starts with a dash", version));
    }
    if !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')) {
        return Err(format!("chart version `{}` contains unexpected characters", version));
    }
    Ok(version)
}

pub fn validate_repo_url(url: &str) -> Result<&str, String> {
    if !url.starts_with("https://") {
        return Err(format!("repo url `{}` is not https", url));
    }
    if url.chars().any(|c| c.is_whitespace() || matches!(c, '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '\\')) {
        return Err(format!("repo url `{}` contains shell metacharacters", url));
    }
    Ok(url)
}

/// Turns `helm get values` output into something safe to feed back with `-f`.
///
/// Two traps here. The default (non-`-o yaml`) output carries a `USER-SUPPLIED VALUES:` header that
/// is not valid YAML. And a release with no user-supplied values yields the literal `null` -- which
/// `helm upgrade -f` accepts and interprets as "set every value to nothing", wiping hostname,
/// letsEncrypt.email and agentTLSMode in one shot.
pub fn normalize_helm_values(raw: &str) -> String {
    let stripped: String = raw
        .lines()
        .filter(|l| l.trim() != "USER-SUPPLIED VALUES:")
        .collect::<Vec<_>>()
        .join("\n");

    let meaningful = stripped
        .lines()
        .any(|l| !l.trim().is_empty() && l.trim() != "null" && !l.trim_start().starts_with('#'));

    if meaningful { format!("{}\n", stripped.trim_end()) } else { "{}\n".to_string() }
}

// ---------------------------------------------------------------------------
// Selecting the target chart
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Target {
    Pinned(String),
    LatestHead,
}

#[derive(Debug, Clone)]
pub struct UpgradePlan {
    pub from_chart: String,
    pub to_version: String,
    pub to_app: String,
    pub published: Option<DateTime<Utc>>,
    pub repo_url: String,
    pub minor: String,
}

pub fn plan_upgrade(
    running: &RunningChart,
    target: &Target,
    repo_url: &str,
    index: &str,
) -> Result<UpgradePlan, Box<dyn std::error::Error>> {
    validate_repo_url(repo_url)?;

    let minor = chart_minor(&running.chart)
        .ok_or_else(|| format!("could not read a minor from the running chart `{}`", running.chart))?;

    let (to_version, to_app, published) = match target {
        Target::Pinned(version) => {
            validate_chart_version(version)?;
            (version.clone(), version.clone(), None)
        }
        Target::LatestHead => {
            let entry: ChartEntry = newest_head_chart(index, &minor)?
                .ok_or_else(|| format!("no head chart published for minor {}", minor))?;
            validate_chart_version(&entry.version)?;
            (entry.version.clone(), entry.app_version.clone(), Some(entry.created))
        }
    };

    // A re-run in the same week would otherwise take a 45-minute snapshot and restart Rancher to
    // arrive exactly where it started.
    if to_version == strip_chart_prefix(&running.chart) {
        return Err(format!("{} is already running; nothing to upgrade", to_version).into());
    }

    Ok(UpgradePlan {
        from_chart: running.chart.clone(),
        to_version,
        to_app,
        published,
        repo_url: repo_url.to_string(),
        minor,
    })
}

/// `rancher-2.16.0-abc-head` -> `2.16.0-abc-head`. `helm list` reports the chart with its name
/// prefixed; the index and `--version` both use the bare version.
pub fn strip_chart_prefix(chart: &str) -> String {
    chart.strip_prefix("rancher-").unwrap_or(chart).to_string()
}

// ---------------------------------------------------------------------------
// World state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterState {
    pub id: String,
    pub display: String,
    pub healthy: bool,
    pub registering: bool,
    /// The full condition tuple, so a cluster that was broken and got *differently* broken is
    /// visible rather than collapsing into "unhealthy before, unhealthy after".
    pub conditions: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub name: String,
    pub downloaded: Option<bool>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseState {
    pub name: String,
    pub chart: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentState {
    pub image: String,
    pub ready: i64,
    pub desired: i64,
    pub updated: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodState {
    pub name: String,
    pub phase: String,
    pub ready: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct WorldState {
    pub clusters: Vec<ClusterState>,
    pub repos: Vec<RepoState>,
    pub releases: Vec<ReleaseState>,
    pub deployment: Option<DeploymentState>,
    pub pods: Vec<PodState>,
    pub values_yaml: Option<String>,
    pub disk_percent: Option<u8>,
}

// One round trip, used identically before and after so the diff compares like with like.
// helm 4 lists every status by default; helm 3 needed `-a` and hides `pending-*` without it. A
// pending release is the single most important thing the pre-flight gate looks for, so `--pending`
// -- which both majors accept -- is captured separately and unioned in rather than trusting either
// version's default. `-a` itself is not usable: helm 4 rejects it outright.
pub const CAPTURE_SCRIPT: &str = r#"
export KUBECONFIG=${KUBECONFIG:-$HOME/.kube/config}
echo '###releases'
helm list -n cattle-system -o json 2>/dev/null
echo '###releases_pending'
helm list -n cattle-system --pending -o json 2>/dev/null
echo '###values'
helm get values rancher -n cattle-system -o yaml 2>/dev/null
echo '###clusters'
kubectl get clusters.management.cattle.io -o json 2>/dev/null
echo '###clusterrepos'
kubectl get clusterrepos.catalog.cattle.io -o json 2>/dev/null
echo '###deploy'
kubectl get deploy rancher -n cattle-system -o json 2>/dev/null
echo '###pods'
kubectl get pods -n cattle-system -o json 2>/dev/null
echo '###disk'
df -P / | tail -n 1
echo '###end'
"#;

pub fn parse_world(sections: &BTreeMap<String, String>) -> WorldState {
    WorldState {
        clusters: sections.get("clusters").map(|s| parse_clusters(s)).unwrap_or_default(),
        repos: sections.get("clusterrepos").map(|s| parse_clusterrepos(s)).unwrap_or_default(),
        releases: merge_releases(
            sections.get("releases").map(|s| parse_releases(s)).unwrap_or_default(),
            sections.get("releases_pending").map(|s| parse_releases(s)).unwrap_or_default(),
        ),
        deployment: sections.get("deploy").and_then(|s| parse_deployment(s)),
        pods: sections.get("pods").map(|s| parse_pods(s)).unwrap_or_default(),
        values_yaml: sections.get("values").map(|s| normalize_helm_values(s)),
        disk_percent: sections.get("disk").and_then(|s| parse_disk_percent(s)),
    }
}

/// Health rules are lifted from `maintain::check_clusters` on purpose: if the two commands ever
/// disagree about what "healthy" means, the weekly report and the upgrade gate contradict
/// each other on the same box in the same minute.
pub fn parse_clusters(raw: &str) -> Vec<ClusterState> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items = json.get("items").and_then(|i| i.as_array()).map(Vec::as_slice).unwrap_or(&[]);

    items
        .iter()
        .map(|item| {
            let id = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
            let display = item.pointer("/spec/displayName").and_then(|v| v.as_str()).unwrap_or(id);

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

            let healthy = if is_local {
                ready.as_deref() == Some("True")
            } else {
                connected.as_deref() == Some("True")
            };

            let registering = !is_local
                && connected.as_deref() == Some("False")
                && agent_deployed.as_deref() == Some("True");

            ClusterState {
                id: id.to_string(),
                display: display.to_string(),
                healthy,
                registering,
                conditions: format!(
                    "Ready={} Connected={} AgentDeployed={}",
                    ready.as_deref().unwrap_or("?"),
                    connected.as_deref().unwrap_or("?"),
                    agent_deployed.as_deref().unwrap_or("?")
                ),
            }
        })
        .collect()
}

/// OCI-backed repos report `OCIDownloaded`; HTTP ones report `Downloaded`. Reading only one of the
/// two marks every repo of the other kind as unknown.
pub fn parse_clusterrepos(raw: &str) -> Vec<RepoState> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items = json.get("items").and_then(|i| i.as_array()).map(Vec::as_slice).unwrap_or(&[]);

    items
        .iter()
        .map(|item| {
            let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("<unnamed>");
            let conditions = item.pointer("/status/conditions").and_then(|v| v.as_array());

            let found = conditions.and_then(|cs| {
                cs.iter().find(|c| {
                    matches!(
                        c.get("type").and_then(|v| v.as_str()),
                        Some("Downloaded") | Some("OCIDownloaded")
                    )
                })
            });

            let downloaded = found
                .and_then(|c| c.get("status").and_then(|v| v.as_str()))
                .map(|s| s == "True");
            let message = found
                .and_then(|c| c.get("message").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();

            RepoState { name: name.to_string(), downloaded, message }
        })
        .collect()
}

pub fn parse_releases(raw: &str) -> Vec<ReleaseState> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    json.as_array()
        .map(|items| {
            items
                .iter()
                .map(|r| ReleaseState {
                    name: r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    chart: r.get("chart").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    status: r.get("status").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Unions the default listing with the explicit `--pending` one, letting a pending entry win.
///
/// Belt and braces across helm majors: whichever listing sees a wedged release, the pre-flight gate
/// gets told about it.
pub fn merge_releases(mut all: Vec<ReleaseState>, pending: Vec<ReleaseState>) -> Vec<ReleaseState> {
    for p in pending {
        match all.iter_mut().find(|r| r.name == p.name) {
            Some(existing) => *existing = p,
            None => all.push(p),
        }
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

pub fn parse_deployment(raw: &str) -> Option<DeploymentState> {
    let json = serde_json::from_str::<serde_json::Value>(raw.trim()).ok()?;
    let image = json
        .pointer("/spec/template/spec/containers/0/image")
        .and_then(|v| v.as_str())?
        .to_string();

    Some(DeploymentState {
        image,
        ready: json.pointer("/status/readyReplicas").and_then(|v| v.as_i64()).unwrap_or(0),
        desired: json.pointer("/spec/replicas").and_then(|v| v.as_i64()).unwrap_or(0),
        updated: json.pointer("/status/updatedReplicas").and_then(|v| v.as_i64()).unwrap_or(0),
    })
}

pub fn parse_pods(raw: &str) -> Vec<PodState> {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items = json.get("items").and_then(|i| i.as_array()).map(Vec::as_slice).unwrap_or(&[]);

    items
        .iter()
        .map(|item| {
            let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let phase = item.pointer("/status/phase").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let statuses = item.pointer("/status/containerStatuses").and_then(|v| v.as_array());

            let ready = statuses
                .map(|cs| !cs.is_empty() && cs.iter().all(|c| c.get("ready").and_then(|v| v.as_bool()) == Some(true)))
                .unwrap_or(false);

            // The waiting reason is what makes a stuck pod legible -- ImagePullBackOff on a chart
            // whose images are still pushing looks identical to "not ready yet" without it.
            let reason = statuses
                .and_then(|cs| {
                    cs.iter().find_map(|c| {
                        c.pointer("/state/waiting/reason")
                            .or_else(|| c.pointer("/state/terminated/reason"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                })
                .unwrap_or_default();

            PodState { name, phase, ready, reason }
        })
        .collect()
}

pub fn parse_disk_percent(raw: &str) -> Option<u8> {
    // Filesystem 1024-blocks Used Available Capacity Mounted-on
    raw.lines()
        .next()?
        .split_whitespace()
        .nth(4)?
        .trim_end_matches('%')
        .parse::<u8>()
        .ok()
}

// ---------------------------------------------------------------------------
// Pre-flight
// ---------------------------------------------------------------------------

/// Reasons to refuse before anything mutates. Empty means clear.
pub fn preflight_findings(before: &WorldState) -> Vec<String> {
    let mut refusals = Vec::new();

    // Starting from a release that is not `deployed` makes the whole differential model
    // meaningless: every "regression" would just be the mess that was already there.
    for release in &before.releases {
        if release.status.starts_with("pending-") || release.status == "failed" {
            refusals.push(format!(
                "helm release `{}` is `{}`; resolve it before upgrading",
                release.name, release.status
            ));
        }
    }

    if let Some(percent) = before.disk_percent
        && percent >= DISK_REFUSAL_PERCENT
    {
        refusals.push(format!(
            "root disk is {}% full; a head upgrade pulls a new image set before releasing the old",
            percent
        ));
    }

    if before.releases.iter().all(|r| r.name != "rancher") {
        refusals.push(
            "no `rancher` helm release found in cattle-system -- the capture may have failed"
                .to_string(),
        );
    }

    refusals
}

/// Warnings that do not block. A chart published minutes ago may have images still pushing.
pub fn plan_warnings(plan: &UpgradePlan, now: DateTime<Utc>) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some(published) = plan.published {
        let age = now.signed_duration_since(published).num_minutes();
        if age < YOUNG_CHART_MINUTES {
            warnings.push(format!(
                "chart was published {} minutes ago; its images may still be pushing to the registry",
                age.max(0)
            ));
        }
    }

    warnings
}

pub fn confirmation_matches(input: &str, name: &str) -> bool {
    input.trim() == name
}

// ---------------------------------------------------------------------------
// Snapshot naming and the restore note
// ---------------------------------------------------------------------------

pub fn snapshot_description(instance: &Instance, now: DateTime<Utc>) -> String {
    format!("pre-{}-upgrade-{}", instance.name, now.format("%Y-%m-%d"))
}

/// Tags matching the manual snapshots already on the volume, so `maintain`'s snapshot check and any
/// future DLM retention policy both see these the same way a hand-taken one is seen.
pub fn snapshot_tags(
    instance: &Instance,
    plan: &UpgradePlan,
    now: DateTime<Utc>,
) -> Vec<(String, String)> {
    vec![
        ("Name".to_string(), snapshot_description(instance, now)),
        ("roa".to_string(), "manual-pre-upgrade".to_string()),
        // The key `provision` already sets, so one tag-scoped IAM policy covers snapshots too.
        ("ManagedBy".to_string(), "roa".to_string()),
        ("roa:reason".to_string(), "upgrade".to_string()),
        ("roa:instance".to_string(), instance.instance_id.clone()),
        ("roa:from-chart".to_string(), plan.from_chart.clone()),
        ("roa:to-chart".to_string(), plan.to_version.clone()),
    ]
}

/// Printed locally on any failure -- never to the box, which may be the thing that is broken.
pub fn restore_instructions(instance: &Instance, snapshot_id: &str, volume_id: &str) -> String {
    format!(
        "  RESTORE POINT\n\
         \x20   snapshot  {snapshot}\n\
         \x20   volume    {volume}\n\
         \x20   instance  {id} ({name}) in {region}\n\
         \n\
         \x20 There is no supported rollback for a head build -- `helm rollback` does not reverse\n\
         \x20 datastore migrations Rancher's controllers may already have run. To go back:\n\
         \x20   1. aws ec2 stop-instances --instance-ids {id} --region {region}\n\
         \x20   2. aws ec2 detach-volume --volume-id {volume} --region {region}\n\
         \x20   3. aws ec2 create-volume --snapshot-id {snapshot} --availability-zone <same-az> --region {region}\n\
         \x20   4. aws ec2 attach-volume --instance-id {id} --volume-id <new-volume> --device /dev/sda1 --region {region}\n\
         \x20   5. aws ec2 start-instances --instance-ids {id} --region {region}\n\
         \x20 Reboot only -- this instance has no Elastic IP and its A record is a literal, so a\n\
         \x20 stop/start reassigns the address and breaks DNS. Re-point Route 53 afterwards.",
        snapshot = snapshot_id,
        volume = volume_id,
        id = instance.instance_id,
        name = instance.name,
        region = instance.region,
    )
}

// ---------------------------------------------------------------------------
// The upgrade script
// ---------------------------------------------------------------------------

/// Renders the script that actually performs the upgrade.
///
/// Only three values are interpolated, and two of them are operator-influenced and validated at the
/// boundary above. Chart values are never interpolated: they are written to a file on the box and
/// passed with `-f`, which sidesteps quoting entirely.
pub fn render_upgrade_script(plan: &UpgradePlan, run_dir: &str, helm_timeout: &str) -> String {
    format!(
        r#"set -euo pipefail
export KUBECONFIG=${{KUBECONFIG:-$HOME/.kube/config}}
mkdir -p '{run_dir}'

# `helm get values` without -o yaml carries a `USER-SUPPLIED VALUES:` header that is not YAML, and
# prints the literal `null` when a release has no user values. Feeding either back with -f is how
# hostname, letsEncrypt.email and agentTLSMode get wiped in one command.
helm get values rancher -n cattle-system -o yaml > '{run_dir}/values.yaml'
if [ ! -s '{run_dir}/values.yaml' ] || [ "$(tr -d '[:space:]' < '{run_dir}/values.yaml')" = "null" ]; then
  echo '{{}}' > '{run_dir}/values.yaml'
fi

# A repo alias differs per user and per week, and helm refuses to re-add an existing name without
# this; reusing a name without updating silently upgrades against a stale cached index.
helm repo add --force-update roa-rancher '{repo_url}'
helm repo update roa-rancher

# NEVER --reuse-values: it keeps the previously computed values and silently drops new chart
# defaults, which is exactly what changes week to week on a head build.
helm upgrade rancher roa-rancher/rancher   --namespace cattle-system   --version '{version}'   --devel   -f '{run_dir}/values.yaml'   --wait --timeout {helm_timeout}

kubectl -n cattle-system rollout status deploy/rancher --timeout={helm_timeout}
"#,
        run_dir = run_dir,
        repo_url = plan.repo_url,
        version = plan.to_version,
        helm_timeout = helm_timeout,
    )
}

/// Writes the upgrade script to the box and starts it detached.
///
/// Not run in the foreground on purpose. `helm upgrade --wait` can be silent for twenty minutes,
/// and if the ssh session dies the SIGHUP kills helm mid-flight and leaves the release in
/// `pending-upgrade` -- the state that blocks the *following* week's run. Detached, a dropped
/// laptop costs the log tail and nothing else.
pub fn render_launcher_script(run_dir: &str, body: &str) -> String {
    format!(
        r#"set -euo pipefail
mkdir -p '{run_dir}'
cat > '{run_dir}/upgrade.sh' <<'ROA_UPGRADE_EOF'
{body}
ROA_UPGRADE_EOF
chmod +x '{run_dir}/upgrade.sh'
setsid nohup bash -c "bash '{run_dir}/upgrade.sh' > '{run_dir}/upgrade.log' 2>&1; echo \$? > '{run_dir}/rc'" < /dev/null > /dev/null 2>&1 &
echo started
"#,
        run_dir = run_dir,
        body = body,
    )
}

pub fn render_poll_script(run_dir: &str, offset: u64) -> String {
    format!(
        r#"tail -c +{offset} '{run_dir}/upgrade.log' 2>/dev/null || true
echo '###rc'
cat '{run_dir}/rc' 2>/dev/null || true
"#,
        run_dir = run_dir,
        offset = offset + 1,
    )
}

pub fn run_dir(now: DateTime<Utc>) -> String {
    format!("/tmp/roa-upgrade-{}", now.format("%Y%m%d-%H%M%S"))
}

// ---------------------------------------------------------------------------
// Differential verification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// Something that was working before this run and is not working now. The only kind that fails.
    Regression,
    /// Broken before, broken after. Reported so it is visible, never fatal -- on a shared box
    /// other people's clusters are permanently broken and are not this command's business.
    PreExisting,
    Recovered,
    /// Not yet where it should be, but the settle window has not expired.
    Unsettled,
    Informational,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub subject: String,
    pub kind: FindingKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settle {
    Waiting,
    Expired,
}

/// The host part of a container image reference.
pub fn registry_host(image: &str) -> Option<String> {
    let first = image.split('/').next()?;
    // A bare `rancher/rancher:tag` has no registry component; the first segment is only a host if
    // it looks like one.
    if first.contains('.') || first.contains(':') { Some(first.to_string()) } else { None }
}

/// Compares two captures of the box and classifies every difference.
///
/// `plan` is `None` when nothing was supposed to change -- the dry-run double-capture -- in which
/// case the target-version assertions are skipped and the result should be empty on a quiet box.
pub fn diff_world(
    before: &WorldState,
    after: &WorldState,
    plan: Option<&UpgradePlan>,
    settle: Settle,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let expired = settle == Settle::Expired;

    // --- clusters ---
    for b in &before.clusters {
        match after.clusters.iter().find(|a| a.id == b.id) {
            // A diff that only walks the "after" list silently ignores a cluster that vanished.
            None => findings.push(Finding {
                subject: format!("cluster/{}", b.display),
                kind: FindingKind::Regression,
                detail: "present before the upgrade, absent after".to_string(),
            }),
            Some(a) => match (b.healthy, a.healthy) {
                (true, true) => {}
                (true, false) => {
                    let kind = if a.registering && !expired {
                        FindingKind::Unsettled
                    } else {
                        FindingKind::Regression
                    };
                    findings.push(Finding {
                        subject: format!("cluster/{}", a.display),
                        kind,
                        detail: if a.registering {
                            format!("was healthy, now registering ({})", a.conditions)
                        } else {
                            format!("was healthy, now {}", a.conditions)
                        },
                    });
                }
                (false, true) => findings.push(Finding {
                    subject: format!("cluster/{}", a.display),
                    kind: FindingKind::Recovered,
                    detail: format!("was unhealthy, now {}", a.conditions),
                }),
                (false, false) => {
                    // Broken before and broken after is not this run's problem -- but broken
                    // *differently* must still be visible, or a cluster that got worse during the
                    // upgrade hides inside "pre-existing".
                    let detail = if a.conditions != b.conditions {
                        format!("still unhealthy, and changed: {} -> {}", b.conditions, a.conditions)
                    } else {
                        format!("unhealthy before and after ({})", a.conditions)
                    };
                    findings.push(Finding {
                        subject: format!("cluster/{}", a.display),
                        kind: FindingKind::PreExisting,
                        detail,
                    });
                }
            },
        }
    }

    for a in &after.clusters {
        if !before.clusters.iter().any(|b| b.id == a.id) {
            findings.push(Finding {
                subject: format!("cluster/{}", a.display),
                kind: FindingKind::Informational,
                detail: "appeared during the run".to_string(),
            });
        }
    }

    // --- ClusterRepos ---
    for b in &before.repos {
        let Some(a) = after.repos.iter().find(|a| a.name == b.name) else {
            continue;
        };
        match (b.downloaded, a.downloaded) {
            (Some(true), Some(true)) => {}
            (Some(true), _) => findings.push(Finding {
                subject: format!("clusterrepo/{}", a.name),
                kind: FindingKind::Regression,
                detail: format!("was downloaded, now not ({})", a.message),
            }),
            (_, Some(true)) => findings.push(Finding {
                subject: format!("clusterrepo/{}", a.name),
                kind: FindingKind::Recovered,
                detail: "now downloading".to_string(),
            }),
            _ => findings.push(Finding {
                subject: format!("clusterrepo/{}", a.name),
                kind: FindingKind::PreExisting,
                detail: format!("not downloading before or after ({})", a.message),
            }),
        }
    }

    // --- helm releases ---
    // A release left `pending-*` is always fatal, even if it was pending before: it is the
    // signature of the rancher-turtles pre-upgrade hook wedge, and it blocks the next run too.
    for a in &after.releases {
        if a.status.starts_with("pending-") {
            findings.push(Finding {
                subject: format!("release/{}", a.name),
                kind: FindingKind::Regression,
                detail: format!("left in `{}`", a.status),
            });
        } else if a.status == "failed"
            && before.releases.iter().any(|b| b.name == a.name && b.status == "deployed")
        {
            findings.push(Finding {
                subject: format!("release/{}", a.name),
                kind: FindingKind::Regression,
                detail: "was deployed, now failed".to_string(),
            });
        }
    }

    // --- deployment ---
    if let (Some(b), Some(a)) = (&before.deployment, &after.deployment) {
        // Differential, so a box that was never on Prime does not fail for not being on it.
        if registry_host(&b.image) != registry_host(&a.image) {
            findings.push(Finding {
                subject: "deployment/rancher".to_string(),
                kind: FindingKind::Regression,
                detail: format!("registry changed: {} -> {}", b.image, a.image),
            });
        }

        if a.updated < a.desired || a.ready < a.desired {
            findings.push(Finding {
                subject: "deployment/rancher".to_string(),
                kind: if expired { FindingKind::Regression } else { FindingKind::Unsettled },
                detail: format!("{}/{} ready, {} updated", a.ready, a.desired, a.updated),
            });
        }

        if let Some(plan) = plan
            && !a.image.contains(&plan.to_app)
        {
            findings.push(Finding {
                subject: "deployment/rancher".to_string(),
                kind: if expired { FindingKind::Regression } else { FindingKind::Unsettled },
                detail: format!("image {} does not carry the target {}", a.image, plan.to_app),
            });
        }
    }

    // --- chart values ---
    // The values round trip is the most dangerous step in the whole command: `helm get values`
    // emits `null` for a release with no user values, and feeding that back with -f wipes
    // hostname, letsEncrypt.email and agentTLSMode in one command. The script guards against it;
    // this checks the guard actually held, because the symptom otherwise is a Rancher that comes
    // up healthy on the wrong hostname with no certificate.
    if let (Some(b), Some(a)) = (&before.values_yaml, &after.values_yaml)
        && b != a
    {
        findings.push(Finding {
            subject: "release/rancher values".to_string(),
            kind: FindingKind::Regression,
            detail: "chart values changed across the upgrade".to_string(),
        });
    }

    // --- pods ---
    // Only pods that are supposed to be running are evidence of anything. Rancher spawns
    // short-lived `helm-operation-*` Job pods for chart work, and they sit in the namespace
    // afterwards as Succeeded (or Failed, which is equally benign -- see the note in the runbook
    // about the post-upgrade `Error` pods). Judging those by container readiness reports every
    // completed job as a failure, on a box where one is almost always present.
    for a in after.pods.iter().filter(|p| is_expected_running(&p.phase)) {
        if !a.ready {
            findings.push(Finding {
                subject: format!("pod/{}", a.name),
                kind: if expired { FindingKind::Regression } else { FindingKind::Unsettled },
                detail: if a.reason.is_empty() { "not ready".to_string() } else { a.reason.clone() },
            });
        }
    }

    findings
}

/// Whether a pod's phase means it is meant to be up right now.
///
/// `Succeeded` and `Failed` are terminal: the pod ran and is done. Only `Running` and `Pending`
/// describe something that should become ready, and both still catch the cases that matter --
/// a crashlooping pod stays `Running`, an ImagePullBackOff one stays `Pending`.
pub fn is_expected_running(phase: &str) -> bool {
    matches!(phase, "Running" | "Pending")
}

/// True when nothing is failing and nothing is still moving.
pub fn is_settled(findings: &[Finding]) -> bool {
    !findings
        .iter()
        .any(|f| matches!(f.kind, FindingKind::Regression | FindingKind::Unsettled))
}

pub fn has_regression(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.kind == FindingKind::Regression)
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

fn capture_world(target: &SshTarget) -> Result<WorldState, Box<dyn std::error::Error>> {
    let output = run_script(target, CAPTURE_SCRIPT)?;

    if !output.stdout.contains("###end") {
        return Err(format!(
            "capture did not complete on {}: {}",
            target.host,
            if output.stderr.is_empty() { "no output".to_string() } else { output.stderr }
        )
        .into());
    }

    Ok(parse_world(&split_sections(&output.stdout)))
}

async fn fetch_index(repo_url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let url = format!("{}/index.yaml", repo_url.trim_end_matches('/'));
    let body = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await?
        .text()
        .await?;
    Ok(body)
}

fn print_findings(findings: &[Finding]) {
    if findings.is_empty() {
        println!("  no differences");
        return;
    }

    let width = findings.iter().map(|f| f.subject.len()).max().unwrap_or(0);

    for finding in findings {
        let label = match finding.kind {
            FindingKind::Regression => "REGRESSION",
            FindingKind::PreExisting => "pre-existing",
            FindingKind::Recovered => "recovered",
            FindingKind::Unsettled => "settling",
            FindingKind::Informational => "info",
        };
        println!("  {:width$}  {:<13} {}", finding.subject, label, finding.detail, width = width);
    }
}

pub async fn upgrade(args: UpgradeArgs) -> Result<(), Box<dyn std::error::Error>> {
    let path = manifest_path();
    let instances = load_instances(&path)?;

    let instance: Instance = instances
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

    // Flag wins, then the repo recorded at provision time, then the default. `maintain` compares
    // against the manifest's value next week, so an upgrade from somewhere else must say so.
    let repo_url = args
        .rancher_repo
        .as_ref()
        .map(|r| r.value())
        .or_else(|| instance.rancher_repo.clone())
        .unwrap_or_else(|| RancherRepo::PrimeLatest.value());

    let target = SshTarget {
        user: &args.ssh_user,
        host: &instance.fqdn,
        key: args.ssh_key.as_deref(),
        keepalive: true,
    };

    println!(
        "{} ({}) {} [{}]\n",
        instance.name, instance.instance_id, instance.fqdn, instance.region
    );

    let before = capture_world(&target)?;

    let refusals = preflight_findings(&before);
    if !refusals.is_empty() {
        for refusal in &refusals {
            eprintln!("  refusing: {}", refusal);
        }
        return Err("pre-flight checks failed; nothing was changed".into());
    }

    let running = before
        .releases
        .iter()
        .find(|r| r.name == "rancher")
        .map(|r| RunningChart { chart: r.chart.clone(), app_version: String::new() })
        .ok_or("no `rancher` release to upgrade")?;

    let chart_target = match &args.rancher_version {
        Some(version) => Target::Pinned(version.clone()),
        None => Target::LatestHead,
    };

    let index = match chart_target {
        Target::LatestHead => fetch_index(&repo_url).await?,
        Target::Pinned(_) => String::new(),
    };

    let plan = plan_upgrade(&running, &chart_target, &repo_url, &index)?;

    println!("  from   {}", plan.from_chart);
    println!("  to     {}", plan.to_version);
    if let Some(published) = plan.published {
        println!("  published {}", published.format("%Y-%m-%d %H:%M UTC"));
    }
    println!("  repo   {}", plan.repo_url);
    println!("  minor  {}", plan.minor);

    for warning in plan_warnings(&plan, Utc::now()) {
        println!("  warning: {}", warning);
    }

    if args.dry_run {
        // Capture the box a second time and diff it against the first. Nothing has been changed,
        // so on a quiet box this must come back empty -- which is what proves every parser and
        // every rule above against real production data before any of it is trusted to gate a
        // real upgrade.
        println!("\n  dry run: re-reading the box to check the differ against a no-op\n");
        std::thread::sleep(std::time::Duration::from_secs(60));

        let after = capture_world(&target)?;
        let findings = diff_world(&before, &after, None, Settle::Expired);

        print_findings(&findings);

        let unexpected: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.kind == FindingKind::Regression)
            .collect();

        println!();
        if unexpected.is_empty() {
            println!("  no regressions across a no-op interval");
        } else {
            println!("  {} unexpected difference(s) across a no-op interval", unexpected.len());
        }

        println!("\n  nothing was changed");
        return Ok(());
    }

    // --- confirmation ------------------------------------------------------
    if !args.yes {
        confirm(&instance, &plan)?;
    }

    // --- restore point ------------------------------------------------------
    let mut restore_note: Option<String> = None;

    if args.skip_snapshot {
        println!("\n  !! --skip-snapshot: no restore point is being taken for this upgrade !!\n");
    } else {
        // The k3s datastore is written to continuously, so a root-volume snapshot of it alone is
        // only crash-consistent. Taking a datastore snapshot first puts a consistent copy *inside*
        // the EBS snapshot, which is the difference between "probably recoverable" and "is".
        println!("\n  taking a k3s datastore snapshot");
        match run_script(&target, DATASTORE_SNAPSHOT_SCRIPT) {
            Ok(out) if out.success => println!("  {}", out.stdout.trim()),
            Ok(out) => println!("  datastore snapshot reported: {}", out.stderr.trim()),
            Err(e) => println!("  could not take a datastore snapshot: {}", e),
        }

        let ec2 = ec2_client(&instance.region).await;
        let volume_id = root_volume_id(&ec2, &instance.instance_id).await?;
        let now = Utc::now();

        let snapshot_id = create_snapshot(
            &ec2,
            &volume_id,
            &snapshot_description(&instance, now),
            &snapshot_tags(&instance, &plan, now),
        )
        .await?;

        println!("  snapshot {} of {} created", snapshot_id, volume_id);
        restore_note = Some(restore_instructions(&instance, &snapshot_id, &volume_id));

        wait_for_snapshot(&ec2, &snapshot_id, std::time::Duration::from_secs(args.snapshot_timeout_minutes * 60))
            .await
            .inspect_err(|_| {
                if let Some(note) = &restore_note {
                    eprintln!("\n{}", note);
                }
            })?;
    }

    // --- the upgrade --------------------------------------------------------
    let dir = run_dir(Utc::now());
    println!("\n  upgrading to {} (log: {}/upgrade.log)\n", plan.to_version, dir);

    let upgrade_result = run_upgrade(&target, &plan, &dir, &args.helm_timeout);

    match &upgrade_result {
        Ok(()) => println!("\n  helm upgrade finished"),
        // Deliberately not fatal here. A helm upgrade that exits non-zero because a post-upgrade
        // hook timed out very often leaves a perfectly healthy Rancher -- that is the
        // rancher-turtles case exactly. Aborting now throws away the report that says which it is.
        Err(e) => println!("\n  helm upgrade reported a failure: {}\n  verifying anyway", e),
    }

    // --- verification -------------------------------------------------------
    println!("\n  verifying (settling for up to {} minutes)\n", args.settle_minutes);

    let findings = verify(
        &target,
        &before,
        &plan,
        std::time::Duration::from_secs(args.settle_minutes * 60),
    )
    .await;

    print_findings(&findings);

    let failed = has_regression(&findings) || upgrade_result.is_err();

    println!();
    if failed {
        if let Some(note) = &restore_note {
            eprintln!("{}\n", note);
        }
        return Err("upgrade did not verify cleanly".into());
    }

    println!("  {} is running {}", instance.name, plan.to_version);

    // `maintain` compares against the manifest's recorded repo next week; if this upgrade came
    // from somewhere else, silently leaving the old value makes next week's report wrong.
    if instance.rancher_repo.as_deref() != Some(plan.repo_url.as_str()) {
        let mut updated = instances.clone();
        if let Some(entry) = updated.iter_mut().find(|i| i.instance_id == instance.instance_id) {
            entry.rancher_repo = Some(plan.repo_url.clone());
        }
        save_instances(&path, &updated)?;
        println!("  recorded {} as the chart repo for {}", plan.repo_url, instance.name);
    }

    Ok(())
}

// A consistent copy of the datastore, taken before the EBS snapshot so it lands inside it. k3s
// uses sqlite by default and etcd when clustered; try etcd first and fall back.
const DATASTORE_SNAPSHOT_SCRIPT: &str = r#"
set -uo pipefail
stamp=$(date -u +%Y%m%d-%H%M%S)
if sudo -n k3s etcd-snapshot save --name "roa-upgrade-${stamp}" >/dev/null 2>&1; then
  echo "etcd snapshot roa-upgrade-${stamp} saved"
elif [ -f /var/lib/rancher/k3s/server/db/state.db ]; then
  sudo -n sqlite3 /var/lib/rancher/k3s/server/db/state.db     ".backup '/var/lib/rancher/k3s/server/db/roa-upgrade-${stamp}.db'" 2>/dev/null     && echo "sqlite datastore copied to roa-upgrade-${stamp}.db"     || echo "no datastore snapshot taken (sqlite3 unavailable)"
else
  echo "no datastore snapshot taken (no etcd, no state.db)"
fi
"#;

fn confirm(instance: &Instance, plan: &UpgradePlan) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;

    println!();
    println!("  {} -> {}", plan.from_chart, plan.to_version);
    println!("  on {} ({})", instance.name, instance.fqdn);
    println!("  There is no supported rollback; the snapshot is the only way back.");

    // A protected instance takes its own name rather than a `y`, so the answer cannot be given by
    // reflex on the box that matters most.
    let prompt = if instance.protected {
        format!("\n  {} is PROTECTED. Type its name to continue: ", instance.name)
    } else {
        "\n  Continue? [y/N]: ".to_string()
    };

    print!("{}", prompt);
    std::io::stdout().flush()?;

    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;

    let accepted = if instance.protected {
        confirmation_matches(&answer, &instance.name)
    } else {
        matches!(answer.trim(), "y" | "Y" | "yes")
    };

    if !accepted {
        return Err("not confirmed; nothing was changed".into());
    }

    Ok(())
}

/// Starts the upgrade detached and streams its log until it records an exit code.
fn run_upgrade(
    target: &SshTarget,
    plan: &UpgradePlan,
    dir: &str,
    helm_timeout: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let body = render_upgrade_script(plan, dir, helm_timeout);
    let launched = run_script(target, &render_launcher_script(dir, &body))?;

    if !launched.success {
        return Err(format!("could not start the upgrade: {}", launched.stderr).into());
    }

    let mut offset: u64 = 0;

    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));

        let polled = run_script(target, &render_poll_script(dir, offset))?;
        let sections = split_sections(&polled.stdout);

        // Everything before the ###rc marker is new log output.
        let log = polled.stdout.split("###rc").next().unwrap_or("");
        if !log.is_empty() {
            for line in log.lines() {
                println!("  | {}", line);
            }
            offset += log.len() as u64;
        }

        if let Some(rc) = sections.get("rc")
            && !rc.trim().is_empty()
        {
            let code: i32 = rc.trim().parse().unwrap_or(-1);
            return if code == 0 {
                Ok(())
            } else {
                Err(format!("upgrade script exited {}", code).into())
            };
        }
    }
}

/// Re-captures the box until it settles or the window expires.
async fn verify(
    target: &SshTarget<'_>,
    before: &WorldState,
    plan: &UpgradePlan,
    settle: std::time::Duration,
) -> Vec<Finding> {
    let deadline = std::time::Instant::now() + settle;

    loop {
        let expired = std::time::Instant::now() >= deadline;

        match capture_world(target) {
            Ok(after) => {
                let findings = diff_world(
                    before,
                    &after,
                    Some(plan),
                    if expired { Settle::Expired } else { Settle::Waiting },
                );

                if is_settled(&findings) || expired {
                    return findings;
                }
            }
            // A capture failure right after a restart is expected, not fatal.
            Err(e) => {
                if expired {
                    return vec![Finding {
                        subject: "capture".to_string(),
                        kind: FindingKind::Regression,
                        detail: format!("could not read the box after the upgrade: {}", e),
                    }];
                }
                println!("  (capture failed, retrying: {})", e);
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- the shell boundary ------------------------------------------------

    // `--rancher-version` is interpolated into a bash script piped to a shell on a box other
    // people share. Anything that can end a quoted string or start a command must not survive.
    #[test]
    fn refuses_a_chart_version_that_could_escape_the_shell() {
        for bad in [
            "2.16.0; rm -rf /",
            "2.16.0`id`",
            "2.16.0$(id)",
            "2.16.0 --set foo=bar",
            "2.16.0'\nrm -rf /",
            "",
            "--devel",
        ] {
            assert!(validate_chart_version(bad).is_err(), "accepted `{}`", bad);
        }

        assert!(validate_chart_version("2.16.0-ae20b74-head").is_ok());
        assert!(validate_chart_version("v2.16.0+up1.2.3").is_ok());
    }

    #[test]
    fn refuses_a_repo_url_that_is_not_https_or_carries_metacharacters() {
        assert!(validate_repo_url("http://charts.example.com").is_err());
        assert!(validate_repo_url("https://charts.example.com; id").is_err());
        assert!(validate_repo_url("https://charts.optimus.rancher.io/server-charts/latest").is_ok());
    }

    // --- the values round trip ---------------------------------------------

    // `helm get values` prints the literal `null` for a release with no user-supplied values, and
    // `helm upgrade -f` a file containing `null` is how you wipe hostname, letsEncrypt.email and
    // agentTLSMode in a single command. The default output also carries a header that is not YAML.
    #[test]
    fn empty_helm_values_become_an_empty_map_not_null() {
        assert_eq!(normalize_helm_values("null\n"), "{}\n");
        assert_eq!(normalize_helm_values(""), "{}\n");
        assert_eq!(normalize_helm_values("USER-SUPPLIED VALUES:\nnull\n"), "{}\n");
        assert_eq!(normalize_helm_values("   \n"), "{}\n");
    }

    #[test]
    fn real_values_survive_the_round_trip_without_the_header() {
        let raw = "USER-SUPPLIED VALUES:\nhostname: shared.ui.rancher.space\nagentTLSMode: system-store\n";
        let out = normalize_helm_values(raw);

        assert!(!out.contains("USER-SUPPLIED"));
        assert!(out.contains("hostname: shared.ui.rancher.space"));
        assert!(out.contains("agentTLSMode: system-store"));
    }

    // --- chart selection ---------------------------------------------------

    const INDEX: &str = r#"
apiVersion: v1
entries:
  rancher:
    - version: 2.16.0-ef86dbf-head
      appVersion: v2.16.0-ef86dbf-head
      created: "2026-09-03T15:51:38.813089555Z"
    - version: 2.16.0-ae20b74-head
      appVersion: v2.16.0-ae20b74-head
      created: "2026-09-04T17:42:45.143532282Z"
    - version: 2.15.2-afd2238-head
      appVersion: v2.15.2-afd2238-head
      created: "2026-09-04T17:52:40.894425906Z"
"#;

    fn running(chart: &str) -> RunningChart {
        RunningChart { chart: chart.to_string(), app_version: format!("v{}", strip_chart_prefix(chart)) }
    }

    // The live 2026-09-04 case: `2.15.2-afd2238-head` was published ten minutes *after* the correct
    // 2.16 target, so a global max-by-date picks a downgrade across a minor. Rancher's chart
    // declares `kubeVersion: < 1.37.0-0`; crossing a minor is not a smaller mistake than a stale
    // chart, it is a worse one.
    #[test]
    fn latest_head_never_crosses_the_running_minor() {
        let plan = plan_upgrade(
            &running("rancher-2.16.0-2f60c05-head"),
            &Target::LatestHead,
            "https://charts.optimus.rancher.io/server-charts/latest",
            INDEX,
        )
        .unwrap();

        assert_eq!(plan.to_version, "2.16.0-ae20b74-head");
        assert_eq!(plan.minor, "2.16");
    }

    // Every head version is `<minor>.0-<sha>-head`, so semver ranks them by an arbitrary hex
    // string: `ef86dbf` sorts above `ae20b74` while being a day older.
    #[test]
    fn never_selects_by_semver() {
        let plan = plan_upgrade(
            &running("rancher-2.16.0-2f60c05-head"),
            &Target::LatestHead,
            "https://charts.optimus.rancher.io/server-charts/latest",
            INDEX,
        )
        .unwrap();

        assert_ne!(plan.to_version, "2.16.0-ef86dbf-head");
    }

    // A re-run in the same week would otherwise take a 45-minute snapshot and restart Rancher to
    // land exactly where it started.
    #[test]
    fn refuses_a_target_that_is_already_running() {
        let err = plan_upgrade(
            &running("rancher-2.16.0-ae20b74-head"),
            &Target::LatestHead,
            "https://charts.optimus.rancher.io/server-charts/latest",
            INDEX,
        )
        .unwrap_err();

        assert!(err.to_string().contains("already running"));
    }

    #[test]
    fn a_pinned_version_is_used_verbatim_and_not_looked_up() {
        let plan = plan_upgrade(
            &running("rancher-2.16.0-2f60c05-head"),
            &Target::Pinned("2.16.0-deadbee-head".to_string()),
            "https://charts.optimus.rancher.io/server-charts/latest",
            INDEX,
        )
        .unwrap();

        assert_eq!(plan.to_version, "2.16.0-deadbee-head");
        assert!(plan.published.is_none());
    }

    // --- pre-flight --------------------------------------------------------

    fn world_with(releases: Vec<ReleaseState>, disk: Option<u8>) -> WorldState {
        WorldState { releases, disk_percent: disk, ..Default::default() }
    }

    fn release(name: &str, status: &str) -> ReleaseState {
        ReleaseState { name: name.to_string(), chart: "rancher-2.16.0-x-head".to_string(), status: status.to_string() }
    }

    // Starting from `pending-upgrade` makes the differential model meaningless, and it is also the
    // signature of the rancher-turtles hook wedge -- upgrading on top of it compounds the problem.
    #[test]
    fn preflight_refuses_a_release_that_is_not_deployed() {
        let world = world_with(vec![release("rancher", "deployed"), release("fleet", "pending-upgrade")], Some(40));
        let refusals = preflight_findings(&world);

        assert_eq!(refusals.len(), 1);
        assert!(refusals[0].contains("fleet"));
    }

    // A head upgrade pulls a new image set before releasing the old one.
    #[test]
    fn preflight_refuses_a_nearly_full_root_disk() {
        let world = world_with(vec![release("rancher", "deployed")], Some(92));
        assert!(preflight_findings(&world).iter().any(|r| r.contains("92%")));

        let world = world_with(vec![release("rancher", "deployed")], Some(57));
        assert!(preflight_findings(&world).is_empty());
    }

    // helm 4 lists every status by default and rejects `-a`; helm 3 needs `-a` and hides pending
    // releases without it. Unioning the explicit `--pending` listing means a wedged release is
    // seen either way -- and a pending entry must win over a stale `deployed` row for the same
    // release, or the gate reads the wrong one.
    #[test]
    fn a_pending_release_survives_the_union_with_the_default_listing() {
        let all = vec![release("rancher", "deployed"), release("fleet", "deployed")];
        let pending = vec![release("fleet", "pending-upgrade")];

        let merged = merge_releases(all, pending);

        assert_eq!(merged.len(), 2);
        assert_eq!(merged.iter().find(|r| r.name == "fleet").unwrap().status, "pending-upgrade");
        assert!(!preflight_findings(&world_with(merged, Some(40))).is_empty());
    }

    #[test]
    fn preflight_refuses_a_box_with_no_rancher_release() {
        let world = world_with(vec![release("fleet", "deployed")], Some(40));
        assert!(preflight_findings(&world).iter().any(|r| r.contains("no `rancher` helm release")));
    }

    // --- confirmation ------------------------------------------------------

    // A protected instance takes the name, not a `y`. The whole point is that it cannot be
    // answered by reflex.
    #[test]
    fn confirmation_requires_the_instance_name() {
        assert!(confirmation_matches("shared", "shared"));
        assert!(confirmation_matches("  shared \n", "shared"));
        assert!(!confirmation_matches("y", "shared"));
        assert!(!confirmation_matches("yes", "shared"));
        assert!(!confirmation_matches("", "shared"));
        assert!(!confirmation_matches("Shared", "shared"));
    }

    // --- parsing -----------------------------------------------------------

    #[test]
    fn reads_cluster_health_the_same_way_maintain_does() {
        let raw = r#"{"items":[
          {"metadata":{"name":"local"},"spec":{"displayName":"local"},
           "status":{"conditions":[{"type":"Ready","status":"True"}]}},
          {"metadata":{"name":"c-m-1"},"spec":{"displayName":"eva-222"},
           "status":{"conditions":[{"type":"Connected","status":"False"},{"type":"AgentDeployed","status":"True"}]}},
          {"metadata":{"name":"c-m-2"},"spec":{"displayName":"test-two"},
           "status":{"conditions":[{"type":"Connected","status":"False"},{"type":"AgentDeployed","status":"False"}]}}
        ]}"#;

        let clusters = parse_clusters(raw);

        assert!(clusters[0].healthy);
        // Connected=False + AgentDeployed=True is mid-registration, not a failure.
        assert!(!clusters[1].healthy && clusters[1].registering);
        assert!(!clusters[2].healthy && !clusters[2].registering);
    }

    // OCI repos report `OCIDownloaded`; HTTP ones report `Downloaded`. Reading only one marks every
    // repo of the other kind unknown -- and `stgregistry` on the shared box is OCI.
    #[test]
    fn reads_both_downloaded_and_ocidownloaded_conditions() {
        let raw = r#"{"items":[
          {"metadata":{"name":"rancher-charts"},"status":{"conditions":[{"type":"Downloaded","status":"True"}]}},
          {"metadata":{"name":"stgregistry"},"status":{"conditions":[{"type":"OCIDownloaded","status":"False","message":"error 500"}]}}
        ]}"#;

        let repos = parse_clusterrepos(raw);

        assert_eq!(repos[0].downloaded, Some(true));
        assert_eq!(repos[1].downloaded, Some(false));
        assert!(repos[1].message.contains("500"));
    }

    #[test]
    fn reads_the_waiting_reason_off_a_stuck_pod() {
        let raw = r#"{"items":[{"metadata":{"name":"rancher-abc"},"status":{"phase":"Pending","containerStatuses":[
          {"ready":false,"state":{"waiting":{"reason":"ImagePullBackOff"}}}]}}]}"#;

        let pods = parse_pods(raw);

        assert!(!pods[0].ready);
        assert_eq!(pods[0].phase, "Pending");
        assert_eq!(pods[0].reason, "ImagePullBackOff");
    }

    // If the values round trip ever regresses, Rancher comes back up healthy on the wrong hostname
    // with no certificate -- which looks like success everywhere except the browser.
    #[test]
    fn chart_values_changing_across_the_upgrade_is_a_regression() {
        let world = |values: &str| WorldState {
            values_yaml: Some(values.to_string()),
            ..Default::default()
        };

        let before = world("hostname: shared.ui.rancher.space\n");

        assert!(has_regression(&diff_world(&before, &world("{}\n"), None, Settle::Expired)));
        assert!(!has_regression(&diff_world(
            &before,
            &world("hostname: shared.ui.rancher.space\n"),
            None,
            Settle::Expired
        )));
    }

    // Rancher leaves a `helm-operation-*` Job pod behind after every chart operation, and it sits
    // there Succeeded with its container not ready. Judging pods purely on container readiness
    // reports that finished job as a regression -- which it did, against the real box, on the
    // first dry run. Terminal phases are not evidence of anything.
    #[test]
    fn a_completed_helm_operation_pod_is_not_a_regression() {
        let pod = |name: &str, phase: &str, ready: bool| PodState {
            name: name.to_string(),
            phase: phase.to_string(),
            ready,
            reason: if ready { String::new() } else { "Completed".to_string() },
        };

        let before = WorldState { pods: vec![pod("rancher-a", "Running", true)], ..Default::default() };
        let after = WorldState {
            pods: vec![pod("rancher-a", "Running", true), pod("helm-operation-nrr92", "Succeeded", false)],
            ..Default::default()
        };

        let findings = diff_world(&before, &after, None, Settle::Expired);
        assert!(!has_regression(&findings), "a Succeeded job pod must not fail the run");

        // A pod that is genuinely meant to be up and is not must still fail.
        let stuck = WorldState {
            pods: vec![pod("rancher-a", "Running", false)],
            ..Default::default()
        };
        assert!(has_regression(&diff_world(&before, &stuck, None, Settle::Expired)));
    }

    #[test]
    fn reads_the_root_percentage_off_df() {
        assert_eq!(parse_disk_percent("/dev/root 101582828 43887560 57678884 44% /\n"), Some(44));
    }
}
