use std::fmt::Write as _;
use std::io::Write as _;

use clap::Parser;
use semver::Version;

use crate::maintain::{ChartEntry, chart_minor, fetch_index, rancher_charts};
use crate::provision::RancherRepo;

#[derive(Parser, Debug)]
pub struct VersionsArgs {
    #[arg(
        long,
        default_value = "latest",
        help = "Rancher Helm chart repo: `latest`, `prime`, `prime-latest` (Prime RC and head), `prime-alpha` (alias `alpha`), `community-alpha`, or `release-<major>.<minor>`"
    )]
    rancher_repo: RancherRepo,

    #[arg(long, help = "Only list charts for this Rancher minor, e.g. `2.16`")]
    minor: Option<String>,

    #[arg(long, default_value_t = false, help = "Only list head charts")]
    head: bool,

    #[arg(long, default_value_t = 20, conflicts_with = "all", help = "Number of charts to list")]
    limit: usize,

    #[arg(long, default_value_t = false, help = "List every matching chart")]
    all: bool,
}

pub async fn versions(args: VersionsArgs) -> Result<(), Box<dyn std::error::Error>> {
    let repo_url = args.rancher_repo.value();
    let index = fetch_index(&repo_url).await?;
    let charts = rancher_charts(&index)?;

    let minor = args.minor.as_deref().map(|m| m.trim_start_matches('v').to_string());
    let mut matching = filter_charts(&charts, minor.as_deref(), args.head);
    matching.sort_by(|a, b| b.created.cmp(&a.created));

    let shown = if args.all { matching.len() } else { args.limit.min(matching.len()) };

    // Rendered whole and written once, so `roa versions | head` closing the pipe early is an
    // ordinary exit rather than println!'s "failed printing to stdout" panic.
    let mut out = String::new();

    writeln!(out, "Rancher charts in {}, newest published first", repo_url)?;
    writeln!(out)?;

    if matching.is_empty() {
        writeln!(out, "  no matching charts")?;
    } else {
        let width = matching[..shown].iter().map(|c| c.version.len()).max().unwrap_or(0);
        writeln!(out, "  {:<20}  {:<width$}  APP VERSION", "PUBLISHED", "VERSION")?;
        for chart in &matching[..shown] {
            writeln!(
            out,
                "  {:<20}  {:<width$}  {}",
                chart.created.format("%Y-%m-%d %H:%M UTC").to_string(),
                chart.version,
                chart.app_version,
            )?;
        }
        if shown < matching.len() {
            writeln!(out)?;
            writeln!(out, "  showing {} of {}; pass --all to list every one", shown, matching.len())?;
        }
    }

    // `roa provision` without --rancher-version installs with `--devel`, which takes the highest
    // semver in the whole repo, whatever --minor says. Head versions rank by their hex SHA, so
    // that is rarely the newest chart and is worth calling out.
    if let Some(unpinned) = unpinned_default(&charts) {
        writeln!(out)?;
        writeln!(
            out,
            "  An unpinned `roa provision` installs {} (published {}).",
            unpinned.version,
            unpinned.created.format("%Y-%m-%d"),
        )?;
        writeln!(out, "  Pass --rancher-version to choose a chart from the list instead.")?;
    }

    match std::io::stdout().lock().write_all(out.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        written => Ok(written?),
    }
}

fn filter_charts<'a>(charts: &'a [ChartEntry], minor: Option<&str>, head: bool) -> Vec<&'a ChartEntry> {
    charts
        .iter()
        .filter(|c| minor.is_none_or(|m| chart_minor(&format!("rancher-{}", c.version)).as_deref() == Some(m)))
        .filter(|c| !head || c.version.ends_with("-head"))
        .collect()
}

// The chart `helm install --devel` resolves to: the highest semver, pre-releases included.
fn unpinned_default(charts: &[ChartEntry]) -> Option<&ChartEntry> {
    charts
        .iter()
        .filter_map(|c| lenient_version(&c.version).map(|v| (v, c)))
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, c)| c)
}

// Helm parses versions leniently, so a patch-less `2.16-<sha>-head` counts as `2.16.0-<sha>-head`.
fn lenient_version(version: &str) -> Option<Version> {
    if let Ok(v) = Version::parse(version) {
        return Some(v);
    }

    let split = version.find(['-', '+']).unwrap_or(version.len());
    let (core, rest) = version.split_at(split);
    if core.split('.').count() != 2 {
        return None;
    }

    Version::parse(&format!("{}.0{}", core, rest)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = r#"
apiVersion: v1
entries:
  rancher:
    - appVersion: v2.16.0-de13353-head
      created: "2026-10-01T17:46:04Z"
      version: 2.16.0-de13353-head
    - appVersion: v2.16.0-ffbd1cc914559930ba6aaa03bc434ef32aefe490-head
      created: "2026-08-21T13:40:30Z"
      version: 2.16.0-ffbd1cc914559930ba6aaa03bc434ef32aefe490-head
    - appVersion: v2.16.0-alpha3
      created: "2026-09-30T09:00:00Z"
      version: 2.16.0-alpha3
    - appVersion: v2.15.4-bbbbbbbb-head
      created: "2026-09-29T09:00:00Z"
      version: 2.15.4-bbbbbbbb-head
"#;

    #[test]
    fn filters_by_minor_and_head() {
        let charts = rancher_charts(INDEX).unwrap();

        let minor = filter_charts(&charts, Some("2.16"), false);
        assert_eq!(minor.len(), 3);

        let heads = filter_charts(&charts, Some("2.16"), true);
        assert_eq!(heads.len(), 2);
        assert!(heads.iter().all(|c| c.version.ends_with("-head")));

        assert_eq!(filter_charts(&charts, None, false).len(), 4);
    }

    #[test]
    fn the_unpinned_default_is_the_highest_semver_not_the_newest() {
        let charts = rancher_charts(INDEX).unwrap();
        let unpinned = unpinned_default(&charts).unwrap();
        assert_eq!(unpinned.version, "2.16.0-ffbd1cc914559930ba6aaa03bc434ef32aefe490-head");
    }

    #[test]
    fn reads_a_patchless_version_the_way_helm_does() {
        assert_eq!(
            lenient_version("2.16-abc-head").unwrap(),
            Version::parse("2.16.0-abc-head").unwrap()
        );
        assert!(lenient_version("not-a-version").is_none());
    }
}
