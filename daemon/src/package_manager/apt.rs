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
        use apt_pkg_native::Cache;

        info!("updating apt cache...");
        let _ = self.runner.run("apt-get", &["update"]);

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
        let output = self.runner.run("apt", &["full-upgrade", "-y"]);

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
}
