use std::collections::BTreeMap;
use std::process::Command;

/// Where and how to reach a box over ssh.
///
/// `host` is deliberately the manifest's `fqdn` rather than its `public_ip`: roa instances have no
/// Elastic IP, so the recorded address goes stale across a stop/start while DNS stays authoritative.
pub struct SshTarget<'a> {
    pub user: &'a str,
    pub host: &'a str,
    pub key: Option<&'a str>,
    /// Adds ServerAlive* options. `helm upgrade --wait` can produce no output for twenty minutes,
    /// and an idle session behind NAT gets reaped -- which SIGHUPs helm mid-flight and leaves the
    /// release in `pending-upgrade`, the exact state that blocks the *following* week's run.
    pub keepalive: bool,
}

pub struct RemoteOutput {
    pub stdout: String,
    pub stderr: String,
    /// The exit status of the remote command, not of `ssh` itself.
    pub success: bool,
    pub status: std::process::ExitStatus,
}

/// Pipes `script` to `bash -s` on the target and collects its output.
///
/// `Err` is reserved for a local failure -- ssh could not be spawned, or its stdin could not be
/// written. A remote command that exits non-zero is a successful *run* and comes back as
/// `Ok(RemoteOutput { success: false, .. })`, because whether that matters is the caller's call:
/// `maintain` swallows it, `upgrade` reports it and keeps going.
pub fn run_script(
    target: &SshTarget,
    script: &str,
) -> Result<RemoteOutput, Box<dyn std::error::Error>> {
    let mut cmd = Command::new("ssh");

    // Never block on a prompt: this is meant to be scriptable.
    cmd.arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("ConnectTimeout=10")
        .arg("-o").arg("StrictHostKeyChecking=accept-new");

    if target.keepalive {
        cmd.arg("-o").arg("ServerAliveInterval=30")
            .arg("-o").arg("ServerAliveCountMax=10");
    }

    if let Some(key) = target.key {
        cmd.arg("-i").arg(key);
    }

    cmd.arg(format!("{}@{}", target.user, target.host))
        .arg("bash -s")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().ok_or("failed to open ssh stdin")?;
        stdin.write_all(script.as_bytes())?;
    }

    let output = child.wait_with_output()?;

    Ok(RemoteOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        success: output.status.success(),
        status: output.status,
    })
}

/// Splits `###name`-delimited output back into a map keyed by section name.
///
/// Batching every check into one script and one round trip is what keeps these commands to a single
/// ssh connection; this is the other half of that arrangement.
pub fn split_sections(output: &str) -> BTreeMap<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_batched_remote_output_into_sections() {
        let output = "###apt\n191;137\n###reboot\nnone\n###k3s_version\nv1.36.3+k3s1\n\n###end\n";
        let sections = split_sections(output);

        assert_eq!(sections.get("apt").unwrap().trim(), "191;137");
        assert_eq!(sections.get("reboot").unwrap().trim(), "none");
        assert_eq!(sections.get("k3s_version").unwrap().trim(), "v1.36.3+k3s1");
        assert!(!sections.contains_key("end"));
    }

    // A section whose command produced nothing still has to exist in the map. Callers distinguish
    // "the command ran and returned nothing" from "the section is missing because the batch died
    // partway", and collapsing the two turns a truncated capture into a silently empty one.
    #[test]
    fn keeps_a_section_that_produced_no_output() {
        let sections = split_sections("###helm\n###clusters\nx\n###end\n");

        assert_eq!(sections.get("helm").unwrap().trim(), "");
        assert!(sections.contains_key("helm"));
        assert_eq!(sections.get("clusters").unwrap().trim(), "x");
    }

    // Output that stops before `###end` -- a dropped connection, or a script killed partway --
    // must still yield the sections that did arrive, so a partial capture is salvageable.
    #[test]
    fn recovers_sections_from_output_truncated_before_the_end_marker() {
        let sections = split_sections("###apt\n1;0\n###reboot\nnone\n");

        assert_eq!(sections.get("apt").unwrap().trim(), "1;0");
        assert_eq!(sections.get("reboot").unwrap().trim(), "none");
    }
}
