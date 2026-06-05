use super::{CommandRunner, PackageManager, RealCommandRunner};
use async_trait::async_trait;
use tracing::{error, info};

pub struct Apt {
    pub runner: Box<dyn CommandRunner>,
}

impl Default for Apt {
    fn default() -> Self {
        Self {
            runner: Box::new(RealCommandRunner),
        }
    }
}

#[async_trait]
impl PackageManager for Apt {
    fn name(&self) -> &str {
        "apt"
    }

    fn version(&self) -> String {
        self.runner
            .run("apt", &["--version"])
            .or_else(|_| self.runner.run("apt-get", &["--version"]))
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
        self.runner.run("apt", &["--version"]).is_ok()
            || self.runner.run("apt-get", &["--version"]).is_ok()
    }

    #[cfg(target_os = "linux")]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        info!("updating apt cache...");
        let output = self.runner.run("sudo", &["apt-get", "update"])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let err_msg = if stderr.contains("Permission denied") || stderr.contains("Are you root") {
                format!("Permission denied when updating apt cache. Are you running as root? stderr: {}", stderr)
            } else {
                format!("Failed to update apt cache: {}. stderr: {}", output.status, stderr)
            };
            error!("{}", err_msg);
            return Err(err_msg.into());
        }

        info!("determining available updates...");
        // Use simulated dist-upgrade to find which packages would be upgraded.
        // This requires sudo because apt needs to lock the cache even for simulated upgrades
        // if run as a non-root user.
        let output = self.runner.run("sudo", &["apt-get", "-s", "dist-upgrade"])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let err_msg = format!("Failed to determine available updates: {}. stderr: {}", output.status, stderr);
            error!("{}", err_msg);
            return Err(err_msg.into());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let updates: Vec<String> = stdout
            .lines()
            .filter(|line| line.starts_with("Inst "))
            .filter_map(|line| line.split_whitespace().nth(1).map(|s| s.to_string()))
            .collect();

        info!("found {} available updates", updates.len());
        Ok(updates)
    }

    #[cfg(not(target_os = "linux"))]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    #[cfg(target_os = "linux")]
    async fn dry_run_upgrade(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get_updates().await
    }

    #[cfg(not(target_os = "linux"))]
    async fn dry_run_upgrade(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("starting apt full upgrade");
        let output = self.runner.run("sudo", &["apt", "full-upgrade", "-y"]);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::io;

    struct MockRunner {
        success: bool,
        stdout: String,
        stderr: String,
    }

    impl CommandRunner for MockRunner {
        fn run(&self, _program: &str, _args: &[&str]) -> io::Result<Output> {
            Ok(Output {
                status: ExitStatus::from_raw(if self.success { 0 } else { 1 << 8 }),
                stdout: self.stdout.as_bytes().to_vec(),
                stderr: self.stderr.as_bytes().to_vec(),
            })
        }
    }

    #[test]
    fn test_apt_name() {
        let apt = Apt::default();
        assert_eq!(apt.name(), "apt");
    }

    #[test]
    fn test_apt_version() {
        let runner = MockRunner {
            success: true,
            stdout: "apt 2.0.2 (amd64)\n".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        assert_eq!(apt.version(), "apt 2.0.2 (amd64)");
    }

    #[test]
    fn test_apt_is_available() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        assert!(apt.is_available());
    }

    #[tokio::test]
    async fn test_apt_dry_run_upgrade_success() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        let result = apt.dry_run_upgrade().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_success() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        assert!(apt.full_upgrade().await.is_ok());
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "dpkg error".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        let result = apt.full_upgrade().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dpkg error"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_apt_get_updates_success() {
        let runner = MockRunner {
            success: true,
            stdout: "Inst pkg1 (1.0)\nInst pkg2 (2.0)\n".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        let result = apt.get_updates().await;
        assert!(result.is_ok());
        let updates = result.unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0], "pkg1");
        assert_eq!(updates[1], "pkg2");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_apt_get_updates_permission_denied() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "E: Could not open lock file /var/lib/apt/lists/lock - open (13: Permission denied)".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Permission denied"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_apt_get_updates_other_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "network error".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
        };
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to update apt cache"));
    }
}
