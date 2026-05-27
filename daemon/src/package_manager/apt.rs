use super::PackageManager;
use async_trait::async_trait;
use std::process::Command;
use tracing::{error, info};

pub struct Apt;

#[async_trait]
impl PackageManager for Apt {
    fn name(&self) -> &str {
        "apt"
    }

    fn version(&self) -> String {
        Command::new("apt")
            .arg("--version")
            .output()
            .or_else(|_| Command::new("apt-get").arg("--version").output())
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            })
            .unwrap_or_else(|_| "unknown".to_string())
    }

    fn is_available(&self) -> bool {
        Command::new("apt")
            .arg("--version")
            .output()
            .is_ok()
            || Command::new("apt-get")
                .arg("--version")
                .output()
                .is_ok()
    }

    #[cfg(target_os = "linux")]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        use apt_pkg_native::Cache;

        info!("updating apt cache...");
        let _ = Command::new("apt-get")
            .arg("update")
            .output();

        info!("determining available updates...");
        let mut updates = Vec::new();
        let mut cache = Cache::get_singleton();

        let mut packages = cache.iter();
        while let Some(pkg) = packages.next() {
            let release = pkg.current_version();
            let candidate = pkg.candidate_version();

            if let (Some(rel), Some(can)) = (release, candidate) {
                if rel != can {
                    updates.push(pkg.name());
                }
            }
        }

        info!("found {} available updates", updates.len());
        Ok(updates)
    }

    #[cfg(not(target_os = "linux"))]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("starting apt full upgrade");
        let output = Command::new("apt")
            .args(["full-upgrade", "-y"])
            .output();

        match output {
            Ok(output) => {
                if output.status.success() {
                    info!("full upgrade completed successfully");
                    Ok(())
                } else {
                    let err_msg = format!(
                        "full upgrade failed with status: {}. stderr: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    );
                    error!("{}", err_msg);
                    Err(err_msg.into())
                }
            }
            Err(e) => {
                let err_msg = format!("failed to execute full upgrade: {e}");
                error!("{}", err_msg);
                Err(err_msg.into())
            }
        }
    }
}
