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
