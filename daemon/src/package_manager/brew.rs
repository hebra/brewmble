use super::{CommandRunner, PackageManager, RealCommandRunner};
use async_trait::async_trait;
use std::io;
use std::process::Output;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{error, info};

pub struct Brew {
    pub runner: Box<dyn CommandRunner>,
    pub last_update: Mutex<Option<Instant>>,
    pub cache_ttl: Duration,
}

impl Default for Brew {
    fn default() -> Self {
        Self {
            runner: Box::new(RealCommandRunner),
            last_update: Mutex::new(None),
            cache_ttl: {
                let ttl_mins = std::env::var("BREWMBLE_BREW_UPDATE_INTERVAL")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(360); // Default to 6 hours
                Duration::from_secs(ttl_mins * 60)
            },
        }
    }
}

impl Brew {
    fn run_brew(&self, args: &[&str]) -> io::Result<Output> {
        let paths = ["brew", "/opt/homebrew/bin/brew", "/usr/local/bin/brew"];
        let mut last_err = None;

        for path in paths {
            match self.runner.run(path, args) {
                Ok(output) => return Ok(output),
                Err(e) => last_err = Some(e),
            }
        }

        Err(last_err.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "brew not found")))
    }
}

#[async_trait]
impl PackageManager for Brew {
    fn name(&self) -> &str {
        "brew"
    }

    fn version(&self) -> String {
        self.run_brew(&["--version"])
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
        self.run_brew(&["--version"]).is_ok()
    }

    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let should_update = {
            let last_update = self.last_update.lock().unwrap();
            match *last_update {
                Some(last) if self.cache_ttl.as_secs() > 0 => last.elapsed() >= self.cache_ttl,
                _ => true,
            }
        };

        if should_update {
            info!("updating brew formulae...");
            let output = self.run_brew(&["update"])?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let err_msg = format!("brew update failed with status: {}. stderr: {}", output.status, stderr);
                error!("{}", err_msg);
                return Err(err_msg.into());
            }
            // Update the last update time
            let mut last_update = self.last_update.lock().unwrap();
            *last_update = Some(Instant::now());
        } else {
            info!(
                "brew cache is still valid (TTL: {} mins), skipping update",
                self.cache_ttl.as_secs() / 60
            );
        }

        info!("determining available updates...");
        let output = self.run_brew(&["outdated", "--quiet"])?;

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
        let output = self.run_brew(&["upgrade"]);

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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
        };
        assert!(brew.full_upgrade().await.is_ok());
    }

    #[tokio::test]
    async fn test_brew_get_updates_caching() {
        use std::sync::Arc;

        struct TrackingRunner {
            calls: Arc<Mutex<Vec<String>>>,
        }
        impl CommandRunner for TrackingRunner {
            fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
                let mut calls = self.calls.lock().unwrap();
                calls.push(format!("{} {}", program, args.join(" ")));
                Ok(Output {
                    status: ExitStatus::from_raw(0),
                    stdout: b"pkg1\n".to_vec(),
                    stderr: b"".to_vec(),
                })
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = TrackingRunner {
            calls: calls.clone(),
        };
        let brew = Brew {
            runner: Box::new(runner),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(60), // 1 minute
        };

        // First call - should run update
        let _ = brew.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(c.iter().any(|s| s.contains("brew update")));
            assert_eq!(c.len(), 2); // update and outdated
        }

        // Second call - should NOT run update
        calls.lock().unwrap().clear();
        let _ = brew.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(!c.iter().any(|s| s.contains("brew update")));
            assert!(c.iter().any(|s| s.contains("outdated")));
            assert_eq!(c.len(), 1);
        }

        // Force update with TTL 0
        let calls2 = Arc::new(Mutex::new(Vec::new()));
        let runner2 = TrackingRunner {
            calls: calls2.clone(),
        };
        let brew2 = Brew {
            runner: Box::new(runner2),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(0),
        };
        let _ = brew2.get_updates().await.unwrap();
        {
            let c = calls2.lock().unwrap();
            assert!(c.iter().any(|s| s.contains("brew update")));
        }
    }
}
