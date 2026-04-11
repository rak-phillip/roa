use clap::Parser;
use crate::instance::{load_instances, manifest_path};

#[derive(Parser, Debug)]
pub struct ListArgs {

}

pub async fn list(_args: ListArgs) -> Result<(), Box<dyn std::error::Error>> {
   let path = manifest_path();
   let instances = load_instances(&path)?;

   for i in instances {
      println!(
         "{} {} {} {}",
         i.instance_id,
         i.name,
         i.public_ip,
         i.fqdn,
      );
   }

   Ok(())
}

