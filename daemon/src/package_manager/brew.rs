use super::PackageManager;
use async_trait::async_trait;
use std::process::Command;
use tracing::{error, info};

pub struct Brew;

#[async_trait]
impl PackageManager for Brew {
    fn name(&self) -> &str {
        "brew"
    }

    fn is_available(&self) -> bool {
        Command::new("brew")
            .arg("--version")
            .output()
            .is_ok()
    }

    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        info!("updating brew formulae...");
        let _ = Command::new("brew")
            .arg("update")
            .output();

        info!("determining available updates...");
        let output = Command::new("brew")
            .args(["outdated", "--quiet"])
            .output()?;

        if !output.status.success() {
            let err_msg = format!(
                "brew outdated failed with status: {}. stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            error!("{}", err_msg);
            return Err(err_msg.into());
        }

        let updates: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect();

        info!("found {} available updates", updates.len());
        Ok(updates)
    }

    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("starting brew upgrade");
        let output = Command::new("brew")
            .arg("upgrade")
            .output();

        match output {
            Ok(output) => {
                if output.status.success() {
                    info!("brew upgrade completed successfully");
                    Ok(())
                } else {
                    let err_msg = format!(
                        "brew upgrade failed with status: {}. stderr: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    );
                    error!("{}", err_msg);
                    Err(err_msg.into())
                }
            }
            Err(e) => {
                let err_msg = format!("failed to execute brew upgrade: {e}");
                error!("{}", err_msg);
                Err(err_msg.into())
            }
        }
    }
}
