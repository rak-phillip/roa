use std::fs;
use std::path::PathBuf;
use serde::{Serialize, Deserialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Instance {
    pub instance_id: String,
    pub name: String,
    pub public_ip: String,
    pub fqdn: String,
    pub security_group_id: String,
    pub hosted_zone_id: String,
    pub region: String,
    pub created_at: String,
    // Chart repo URL Rancher was installed from. Optional so manifests written before this
    // field existed still load; `maintain` falls back to its own flag when it is absent.
    #[serde(default)]
    pub rancher_repo: Option<String>,
    #[serde(default)]
    pub protected: bool,
}

pub fn manifest_path() -> PathBuf {
    let home = home::home_dir().expect("Could not find HOME directory");
    home.join(".config/roa/instances.json")
}

pub fn load_instances(path: &PathBuf) -> Result<Vec<Instance>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(vec![])
    }

    let contents = std::fs::read_to_string(path)?;
    let instances = serde_json::from_str(&contents)?;

    Ok(instances)
}

pub fn save_instances(path: &PathBuf, instances: &Vec<Instance>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(instances)?;
    fs::write(path, json)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live manifest predates both `rancher_repo` and `protected`. Neither addition may make an
    // existing entry unreadable, or `maintain` and `terminate` both stop working on it.
    #[test]
    fn manifest_without_new_fields_still_loads() {
        let json = r#"[{
            "instance_id": "i-0bbed900e95569f45",
            "name": "shared",
            "public_ip": "34.221.165.41",
            "fqdn": "shared.ui.rancher.space",
            "security_group_id": "sg-08cf71467dc6597f0",
            "hosted_zone_id": "Z09333683MHTBDZFF466H",
            "region": "us-west-2",
            "created_at": "2026-08-13 21:46:39 UTC"
        }]"#;

        let instances: Vec<Instance> = serde_json::from_str(json).expect("legacy entry must load");

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].rancher_repo, None);
        assert!(!instances[0].protected, "an absent flag means unprotected");
    }

    #[test]
    fn protected_round_trips() {
        let json = r#"[{
            "instance_id": "i-1",
            "name": "shared",
            "public_ip": "203.0.113.1",
            "fqdn": "shared.ui.rancher.space",
            "security_group_id": "sg-0",
            "hosted_zone_id": "Z0",
            "region": "us-west-2",
            "created_at": "2026-08-13 21:46:39 UTC",
            "protected": true
        }]"#;

        let instances: Vec<Instance> = serde_json::from_str(json).unwrap();
        assert!(instances[0].protected);

        let encoded = serde_json::to_string(&instances).unwrap();
        let decoded: Vec<Instance> = serde_json::from_str(&encoded).unwrap();
        assert!(decoded[0].protected);
    }
}
