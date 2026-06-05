use super::{CommandRunner, PackageManager, RealCommandRunner};
use async_trait::async_trait;
#[cfg(any(target_os = "linux", test))]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", test))]
use std::time::{Duration, Instant};
use tracing::{error, info};

pub struct Apt {
    pub runner: Box<dyn CommandRunner>,
    #[cfg(any(target_os = "linux", test))]
    pub last_update: Mutex<Option<Instant>>,
    #[cfg(any(target_os = "linux", test))]
    pub cache_ttl: Duration,
}

impl Default for Apt {
    fn default() -> Self {
        Self {
            runner: Box::new(RealCommandRunner),
            #[cfg(any(target_os = "linux", test))]
            last_update: Mutex::new(None),
            #[cfg(any(target_os = "linux", test))]
            cache_ttl: {
                let ttl_mins = std::env::var("BREWMBLE_APT_UPDATE_INTERVAL")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(360); // Default to 6 hours
                Duration::from_secs(ttl_mins * 60)
            },
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

    #[cfg(any(target_os = "linux", test))]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        let should_update = {
            let last_update = self.last_update.lock().unwrap();
            match *last_update {
                Some(last) if self.cache_ttl.as_secs() > 0 => last.elapsed() >= self.cache_ttl,
                _ => true,
            }
        };

        if should_update {
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
            // Update the last update time
            let mut last_update = self.last_update.lock().unwrap();
            *last_update = Some(Instant::now());
        } else {
            info!(
                "apt cache is still valid (TTL: {} mins), skipping update",
                self.cache_ttl.as_secs() / 60
            );
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

    #[cfg(all(not(target_os = "linux"), not(test)))]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    #[cfg(any(target_os = "linux", test))]
    async fn dry_run_upgrade(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get_updates().await
    }

    #[cfg(all(not(target_os = "linux"), not(test)))]
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
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
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
        };
        let result = apt.full_upgrade().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("dpkg error"));
    }

    #[cfg(any(target_os = "linux", test))]
    #[tokio::test]
    async fn test_apt_get_updates_success() {
        let runner = MockRunner {
            success: true,
            stdout: "Inst pkg1 (1.0)\nInst pkg2 (2.0)\n".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
        };
        let result = apt.get_updates().await;
        assert!(result.is_ok());
        let updates = result.unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0], "pkg1");
        assert_eq!(updates[1], "pkg2");
    }

    #[cfg(any(target_os = "linux", test))]
    #[tokio::test]
    async fn test_apt_get_updates_permission_denied() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "E: Could not open lock file /var/lib/apt/lists/lock - open (13: Permission denied)".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
        };
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Permission denied"));
    }

    #[cfg(any(target_os = "linux", test))]
    #[tokio::test]
    async fn test_apt_get_updates_other_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "network error".to_string(),
        };
        let apt = Apt {
            runner: Box::new(runner),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(360 * 60),
        };
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Failed to update apt cache"));
    }

    #[tokio::test]
    async fn test_apt_get_updates_caching() {
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
                    stdout: b"Inst pkg1 (1.0)\n".to_vec(),
                    stderr: b"".to_vec(),
                })
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = TrackingRunner { calls: calls.clone() };
        let apt = Apt {
            runner: Box::new(runner),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(60), // 1 minute
        };

        // First call - should run update
        let _ = apt.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(c.iter().any(|s| s.contains("apt-get update")));
            assert_eq!(c.len(), 2); // update and dist-upgrade
        }

        // Second call - should NOT run update
        calls.lock().unwrap().clear();
        let _ = apt.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(!c.iter().any(|s| s.contains("apt-get update")));
            assert!(c.iter().any(|s| s.contains("dist-upgrade")));
            assert_eq!(c.len(), 1);
        }

        // Force update with TTL 0
        let runner2 = TrackingRunner { calls: calls.clone() };
        let apt2 = Apt {
            runner: Box::new(runner2),
            last_update: Mutex::new(None),
            cache_ttl: Duration::from_secs(0),
        };
        calls.lock().unwrap().clear();
        let _ = apt2.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(c.iter().any(|s| s.contains("apt-get update")));
        }
    }

    #[test]
    fn test_apt_default_config() {
        // We use a mutex to ensure we don't interfere with other tests that might read env
        // but since we are just testing the default implementation it should be fine
        // if we run this test specifically or with --test-threads=1
        unsafe {
            std::env::set_var("BREWMBLE_APT_UPDATE_INTERVAL", "123");
        }
        let apt = Apt::default();
        assert_eq!(apt.cache_ttl, Duration::from_secs(123 * 60));
        unsafe {
            std::env::remove_var("BREWMBLE_APT_UPDATE_INTERVAL");
        }

        let apt_default = Apt::default();
        assert_eq!(apt_default.cache_ttl, Duration::from_secs(360 * 60));
    }
}
