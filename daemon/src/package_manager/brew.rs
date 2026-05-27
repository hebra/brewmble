use super::{CommandRunner, PackageManager, RealCommandRunner};
use async_trait::async_trait;
use tracing::{error, info};

pub struct Brew {
    pub runner: Box<dyn CommandRunner>,
}

impl Default for Brew {
    fn default() -> Self {
        Self {
            runner: Box::new(RealCommandRunner),
        }
    }
}

#[async_trait]
impl PackageManager for Brew {
    fn name(&self) -> &str {
        "brew"
    }

    fn version(&self) -> String {
        self.runner
            .run("brew", &["--version"])
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
        self.runner.run("brew", &["--version"]).is_ok()
    }

    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        info!("updating brew formulae...");
        let _ = self.runner.run("brew", &["update"]);

        info!("determining available updates...");
        let output = self.runner.run("brew", &["outdated", "--quiet"])?;

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
            .filter(|s| !s.is_empty())
            .collect();

        info!("found {} available updates", updates.len());
        Ok(updates)
    }

    async fn dry_run_upgrade(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get_updates().await
    }

    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("starting brew upgrade");
        let output = self.runner.run("brew", &["upgrade"]);

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
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            if program == "brew" && args.contains(&"outdated") {
                if self.success {
                    return Ok(Output {
                        status: ExitStatus::from_raw(0),
                        stdout: self.stdout.as_bytes().to_vec(),
                        stderr: "".as_bytes().to_vec(),
                    });
                } else {
                    return Ok(Output {
                        status: ExitStatus::from_raw(1 << 8),
                        stdout: "".as_bytes().to_vec(),
                        stderr: self.stderr.as_bytes().to_vec(),
                    });
                }
            }

            Ok(Output {
                status: ExitStatus::from_raw(if self.success { 0 } else { 1 << 8 }),
                stdout: self.stdout.as_bytes().to_vec(),
                stderr: self.stderr.as_bytes().to_vec(),
            })
        }
    }

    #[test]
    fn test_brew_name() {
        let brew = Brew::default();
        assert_eq!(brew.name(), "brew");
    }

    #[test]
    fn test_brew_version() {
        let runner = MockRunner {
            success: true,
            stdout: "Homebrew 3.3.16\n".to_string(),
            stderr: "".to_string(),
        };
        let brew = Brew {
            runner: Box::new(runner),
        };
        assert_eq!(brew.version(), "Homebrew 3.3.16");
    }

    #[tokio::test]
    async fn test_brew_get_updates_success() {
        let runner = MockRunner {
            success: true,
            stdout: "pkg1\npkg2\n".to_string(),
            stderr: "".to_string(),
        };
        let brew = Brew {
            runner: Box::new(runner),
        };
        let updates = brew.get_updates().await.unwrap();
        assert_eq!(updates, vec!["pkg1", "pkg2"]);
    }

    #[tokio::test]
    async fn test_brew_get_updates_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "brew error".to_string(),
        };
        let brew = Brew {
            runner: Box::new(runner),
        };
        let result = brew.get_updates().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("brew error"));
    }

    #[tokio::test]
    async fn test_brew_dry_run_upgrade_success() {
        let runner = MockRunner {
            success: true,
            stdout: "pkg1\npkg2\n".to_string(),
            stderr: "".to_string(),
        };
        let brew = Brew {
            runner: Box::new(runner),
        };
        let updates = brew.dry_run_upgrade().await.unwrap();
        assert_eq!(updates, vec!["pkg1", "pkg2"]);
    }

    #[tokio::test]
    async fn test_brew_full_upgrade_success() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let brew = Brew {
            runner: Box::new(runner),
        };
        assert!(brew.full_upgrade().await.is_ok());
    }
}
