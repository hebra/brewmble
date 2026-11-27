use super::{CommandRunner, PackageManager, RealCommandRunner};
use async_trait::async_trait;
use std::io;
use std::process::Output;
use std::sync::Arc;
#[cfg(any(target_os = "linux", test))]
use std::sync::Mutex;
#[cfg(any(target_os = "linux", test))]
use std::time::{Duration, Instant};
#[cfg(any(target_os = "linux", test))]
use tokio::sync::watch;
use tracing::{error, info, warn};

#[cfg(any(target_os = "linux", test))]
type UpdateCheckResult = Result<Vec<String>, String>;

pub struct Apt {
    pub runner: Arc<dyn CommandRunner>,
    pub auto_clean: bool,
    pub auto_remove: bool,
    #[cfg(any(target_os = "linux", test))]
    pub last_update: Arc<Mutex<Option<Instant>>>,
    #[cfg(any(target_os = "linux", test))]
    pub cache_ttl: Duration,
    /// The update check currently running, shared by every caller that
    /// arrives before it finishes.
    #[cfg(any(target_os = "linux", test))]
    in_flight: Mutex<Option<watch::Receiver<Option<UpdateCheckResult>>>>,
}

impl Apt {
    pub fn new(auto_clean: bool, auto_remove: bool, update_interval_mins: u64) -> Self {
        Self {
            runner: Arc::new(RealCommandRunner),
            auto_clean,
            auto_remove,
            #[cfg(any(target_os = "linux", test))]
            last_update: Arc::new(Mutex::new(None)),
            #[cfg(any(target_os = "linux", test))]
            cache_ttl: Duration::from_secs(update_interval_mins * 60),
            #[cfg(any(target_os = "linux", test))]
            in_flight: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_runner(
        runner: Arc<dyn CommandRunner>,
        auto_clean: bool,
        auto_remove: bool,
        cache_ttl: Duration,
    ) -> Self {
        Self {
            runner,
            cache_ttl,
            ..Self::new(auto_clean, auto_remove, 0)
        }
    }

    async fn run_async(
        &self,
        program: &'static str,
        args: &'static [&'static str],
    ) -> io::Result<Output> {
        run_blocking(self.runner.clone(), program, args).await
    }
}

async fn run_blocking(
    runner: Arc<dyn CommandRunner>,
    program: &'static str,
    args: &'static [&'static str],
) -> io::Result<Output> {
    tokio::task::spawn_blocking(move || runner.run(program, args))
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
}

/// Refreshes the apt cache if its TTL has expired, then lists available
/// upgrades. Runs as its own task so it finishes even if every request
/// waiting on it is dropped (e.g. by the HTTP timeout); `sudo apt-get`
/// can't be killed from the unprivileged daemon anyway.
#[cfg(any(target_os = "linux", test))]
async fn check_updates(
    runner: Arc<dyn CommandRunner>,
    last_update: Arc<Mutex<Option<Instant>>>,
    cache_ttl: Duration,
) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let should_update = {
        let last_update = last_update.lock().unwrap_or_else(|p| p.into_inner());
        match *last_update {
            Some(last) if cache_ttl.as_secs() > 0 => last.elapsed() >= cache_ttl,
            _ => true,
        }
    };

    if should_update {
        info!("updating apt cache...");
        let output = run_blocking(runner.clone(), "sudo", &["apt-get", "update"]).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let err_msg = if stderr.contains("Permission denied") || stderr.contains("Are you root")
            {
                format!(
                    "Permission denied when updating apt cache. Are you running as root? stderr: {}",
                    stderr
                )
            } else {
                format!(
                    "Failed to update apt cache: {}. stderr: {}",
                    output.status, stderr
                )
            };
            error!("{}", err_msg);
            return Err(err_msg.into());
        }
        // Update the last update time
        let mut last_update = last_update.lock().unwrap_or_else(|p| p.into_inner());
        *last_update = Some(Instant::now());
    } else {
        info!(
            "apt cache is still valid (TTL: {} mins), skipping update",
            cache_ttl.as_secs() / 60
        );
    }

    info!("determining available updates...");
    // Use simulated dist-upgrade to find which packages would be upgraded.
    // This requires sudo because apt needs to lock the cache even for simulated upgrades
    // if run as a non-root user.
    let output = run_blocking(runner, "sudo", &["apt-get", "-s", "dist-upgrade"]).await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let err_msg = format!(
            "Failed to determine available updates: {}. stderr: {}",
            output.status, stderr
        );
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

impl Default for Apt {
    fn default() -> Self {
        Self::new(false, false, 360)
    }
}

/// Must match the `dist-upgrade` entry of the documented sudoers rule exactly.
pub(crate) const FULL_UPGRADE_SUDO_ARGS: &[&str] = &[
    "apt-get",
    "dist-upgrade",
    "-y",
    "-o",
    "Dpkg::Options::=--force-confdef",
    "-o",
    "Dpkg::Options::=--force-confold",
];

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
        let mut rx = {
            let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
            match in_flight.as_ref() {
                Some(rx) if rx.borrow().is_none() && rx.has_changed().is_ok() => {
                    info!("joining apt update check already in progress");
                    rx.clone()
                }
                _ => {
                    let (tx, rx) = watch::channel(None);
                    let check = check_updates(
                        self.runner.clone(),
                        self.last_update.clone(),
                        self.cache_ttl,
                    );
                    tokio::spawn(async move {
                        let _ = tx.send(Some(check.await.map_err(|e| e.to_string())));
                    });
                    *in_flight = Some(rx.clone());
                    rx
                }
            }
        };

        let result = rx
            .wait_for(Option::is_some)
            .await
            .map_err(|_| "apt update check ended without a result")?;
        match result.as_ref() {
            Some(Ok(updates)) => Ok(updates.clone()),
            Some(Err(e)) => Err(e.clone().into()),
            None => unreachable!("wait_for only returns once a result is set"),
        }
    }

    #[cfg(all(not(target_os = "linux"), not(test)))]
    async fn get_updates(&self) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    #[cfg(any(target_os = "linux", test))]
    async fn dry_run_upgrade(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        self.get_updates().await
    }

    #[cfg(all(not(target_os = "linux"), not(test)))]
    async fn dry_run_upgrade(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![])
    }

    async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("starting apt full upgrade");
        // No DEBIAN_FRONTEND: sudo's env_reset would drop it, and without a tty
        // debconf already falls back to the defaults (see daemon/README.md).
        let output = self.run_async("sudo", FULL_UPGRADE_SUDO_ARGS).await;

        match output {
            Ok(output) => {
                if output.status.success() {
                    info!("full upgrade completed successfully");
                    self.run_cleanup();
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

    fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("running apt autoclean");
        let output = self.runner.run("sudo", &["apt-get", "autoclean"])?;
        if output.status.success() {
            info!("apt autoclean completed successfully");
            Ok(())
        } else {
            let err_msg = format!(
                "apt autoclean failed with status: {}. stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            error!("{}", err_msg);
            Err(err_msg.into())
        }
    }

    fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("running apt autoremove");
        let output = self.runner.run("sudo", &["apt-get", "autoremove", "-y"])?;
        if output.status.success() {
            info!("apt autoremove completed successfully");
            Ok(())
        } else {
            let err_msg = format!(
                "apt autoremove failed with status: {}. stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            error!("{}", err_msg);
            Err(err_msg.into())
        }
    }
}

impl Apt {
    fn run_cleanup(&self) {
        if self.auto_clean {
            if let Err(e) = self.auto_clean() {
                warn!("auto-clean failed, continuing: {e}");
            }
        }
        if self.auto_remove {
            if let Err(e) = self.auto_remove() {
                warn!("auto-remove failed, continuing: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

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
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
        assert_eq!(apt.version(), "apt 2.0.2 (amd64)");
    }

    #[test]
    fn test_apt_is_available() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
        assert!(apt.is_available());
    }

    #[tokio::test]
    async fn test_apt_dry_run_upgrade_success() {
        let runner = MockRunner {
            success: true,
            stdout: "".to_string(),
            stderr: "".to_string(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
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
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
        assert!(apt.full_upgrade().await.is_ok());
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "dpkg error".to_string(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
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
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
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
            stderr:
                "E: Could not open lock file /var/lib/apt/lists/lock - open (13: Permission denied)"
                    .to_string(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Permission denied")
        );
    }

    #[cfg(any(target_os = "linux", test))]
    #[tokio::test]
    async fn test_apt_get_updates_other_failure() {
        let runner = MockRunner {
            success: false,
            stdout: "".to_string(),
            stderr: "network error".to_string(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );
        let result = apt.get_updates().await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to update apt cache")
        );
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
        let runner = TrackingRunner {
            calls: calls.clone(),
        };
        let apt = Apt::with_runner(Arc::new(runner), false, false, Duration::from_secs(60));

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
        let runner2 = TrackingRunner {
            calls: calls.clone(),
        };
        let apt2 = Apt::with_runner(Arc::new(runner2), false, false, Duration::from_secs(0));
        calls.lock().unwrap().clear();
        let _ = apt2.get_updates().await.unwrap();
        {
            let c = calls.lock().unwrap();
            assert!(c.iter().any(|s| s.contains("apt-get update")));
        }
    }

    #[test]
    fn test_apt_default_config() {
        let apt = Apt::new(true, true, 120);
        assert_eq!(apt.cache_ttl, Duration::from_secs(120 * 60));
        assert!(apt.auto_clean);
        assert!(apt.auto_remove);

        let apt_default = Apt::default();
        assert_eq!(apt_default.cache_ttl, Duration::from_secs(360 * 60));
        assert!(!apt_default.auto_clean);
        assert!(!apt_default.auto_remove);
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_runs_cleanup_when_enabled() {
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
                    stdout: b"".to_vec(),
                    stderr: b"".to_vec(),
                })
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = TrackingRunner {
            calls: calls.clone(),
        };
        let apt = Apt::with_runner(Arc::new(runner), true, true, Duration::from_secs(360 * 60));

        assert!(apt.full_upgrade().await.is_ok());

        let c = calls.lock().unwrap();
        assert!(c.iter().any(|s| s == "sudo apt-get dist-upgrade -y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold"));
        assert!(c.iter().any(|s| s.contains("apt-get autoclean")));
        assert!(c.iter().any(|s| s.contains("apt-get autoremove")));
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_skips_cleanup_when_disabled() {
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
                    stdout: b"".to_vec(),
                    stderr: b"".to_vec(),
                })
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = TrackingRunner {
            calls: calls.clone(),
        };
        let apt = Apt::with_runner(
            Arc::new(runner),
            false,
            false,
            Duration::from_secs(360 * 60),
        );

        assert!(apt.full_upgrade().await.is_ok());

        let c = calls.lock().unwrap();
        assert!(c.iter().any(|s| s.contains("apt-get dist-upgrade")));
        assert!(!c.iter().any(|s| s.contains("apt-get autoclean")));
        assert!(!c.iter().any(|s| s.contains("apt-get autoremove")));
    }

    #[tokio::test]
    async fn test_apt_full_upgrade_cleanup_failure_does_not_fail_upgrade() {
        use std::sync::Arc;

        struct FailingCleanupRunner {
            calls: Arc<Mutex<Vec<String>>>,
        }
        impl CommandRunner for FailingCleanupRunner {
            fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
                let mut calls = self.calls.lock().unwrap();
                calls.push(format!("{} {}", program, args.join(" ")));
                if program == "sudo" && args.contains(&"autoclean") {
                    Ok(Output {
                        status: ExitStatus::from_raw(1 << 8),
                        stdout: b"".to_vec(),
                        stderr: b"autoclean failed".to_vec(),
                    })
                } else {
                    Ok(Output {
                        status: ExitStatus::from_raw(0),
                        stdout: b"".to_vec(),
                        stderr: b"".to_vec(),
                    })
                }
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingCleanupRunner {
            calls: calls.clone(),
        };
        let apt = Apt::with_runner(Arc::new(runner), true, false, Duration::from_secs(360 * 60));

        assert!(apt.full_upgrade().await.is_ok());

        let c = calls.lock().unwrap();
        assert!(c.iter().any(|s| s.contains("apt-get dist-upgrade")));
        assert!(c.iter().any(|s| s.contains("apt-get autoclean")));
    }

    #[test]
    fn test_full_upgrade_command_matches_documented_sudoers_rule() {
        let readme = include_str!("../../README.md");
        let documented = "/usr/bin/apt-get dist-upgrade -y -o Dpkg\\:\\:Options\\:\\:=--force-confdef -o Dpkg\\:\\:Options\\:\\:=--force-confold,";
        assert!(readme.contains(documented));
        let expected = documented
            .trim_start_matches("/usr/bin/")
            .trim_end_matches(',')
            .replace("\\:", ":");
        assert_eq!(FULL_UPGRADE_SUDO_ARGS.join(" "), expected);
        assert!(
            !FULL_UPGRADE_SUDO_ARGS
                .iter()
                .any(|a| a.contains("DEBIAN_FRONTEND"))
        );
    }

    /// Records calls and blocks every command until the gate opens.
    struct GatedRunner {
        calls: Arc<Mutex<Vec<String>>>,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
        update_succeeds: bool,
    }

    impl GatedRunner {
        fn new(update_succeeds: bool) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                gate: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
                update_succeeds,
            }
        }

        fn open(gate: &(Mutex<bool>, std::sync::Condvar)) {
            *gate.0.lock().unwrap() = true;
            gate.1.notify_all();
        }
    }

    impl CommandRunner for GatedRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<Output> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} {}", program, args.join(" ")));
            let (open, cvar) = &*self.gate;
            let _open = cvar
                .wait_while(open.lock().unwrap(), |open| !*open)
                .unwrap();
            let ok = self.update_succeeds || !args.contains(&"update");
            Ok(Output {
                status: ExitStatus::from_raw(if ok { 0 } else { 1 << 8 }),
                stdout: b"Inst pkg1 [0.9] (1.0)\n".to_vec(),
                stderr: b"".to_vec(),
            })
        }
    }

    async fn wait_for_calls(calls: &Mutex<Vec<String>>, n: usize) {
        while calls.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apt_concurrent_get_updates_share_one_check() {
        let runner = GatedRunner::new(true);
        let calls = runner.calls.clone();
        let gate = runner.gate.clone();
        let apt = Apt::with_runner(Arc::new(runner), false, false, Duration::from_secs(60));

        let (status, dry_run, ()) = tokio::join!(apt.get_updates(), apt.dry_run_upgrade(), async {
            wait_for_calls(&calls, 1).await;
            GatedRunner::open(&gate);
        });

        assert_eq!(status.unwrap(), vec!["pkg1"]);
        assert_eq!(dry_run.unwrap(), vec!["pkg1"]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["sudo apt-get update", "sudo apt-get -s dist-upgrade"]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apt_timed_out_check_keeps_running_and_is_joined() {
        let runner = GatedRunner::new(true);
        let calls = runner.calls.clone();
        let gate = runner.gate.clone();
        let apt = Apt::with_runner(Arc::new(runner), false, false, Duration::from_secs(60));

        // Simulates the HTTP TimeoutLayer dropping the request future.
        let timed_out = tokio::time::timeout(Duration::from_millis(50), apt.get_updates()).await;
        assert!(timed_out.is_err());

        let (retry, ()) = tokio::join!(apt.get_updates(), async {
            wait_for_calls(&calls, 1).await;
            GatedRunner::open(&gate);
        });

        assert_eq!(retry.unwrap(), vec!["pkg1"]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["sudo apt-get update", "sudo apt-get -s dist-upgrade"]
        );
        assert!(apt.last_update.lock().unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apt_abandoned_check_still_records_successful_update() {
        let runner = GatedRunner::new(true);
        let calls = runner.calls.clone();
        let gate = runner.gate.clone();
        let apt = Apt::with_runner(Arc::new(runner), false, false, Duration::from_secs(60));

        let timed_out = tokio::time::timeout(Duration::from_millis(50), apt.get_updates()).await;
        assert!(timed_out.is_err());
        GatedRunner::open(&gate);
        wait_for_calls(&calls, 2).await;
        while apt
            .in_flight
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .borrow()
            .is_none()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(apt.last_update.lock().unwrap().is_some());

        // The next request starts a fresh check but reuses the cached update.
        calls.lock().unwrap().clear();
        assert_eq!(apt.get_updates().await.unwrap(), vec!["pkg1"]);
        assert_eq!(*calls.lock().unwrap(), vec!["sudo apt-get -s dist-upgrade"]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_apt_failed_update_shares_error_and_leaves_cache_stale() {
        let runner = GatedRunner::new(false);
        let calls = runner.calls.clone();
        let gate = runner.gate.clone();
        let apt = Apt::with_runner(Arc::new(runner), false, false, Duration::from_secs(60));

        let (first, second, ()) = tokio::join!(apt.get_updates(), apt.get_updates(), async {
            wait_for_calls(&calls, 1).await;
            GatedRunner::open(&gate);
        });

        for result in [first, second] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Failed to update apt cache")
            );
        }
        assert_eq!(*calls.lock().unwrap(), vec!["sudo apt-get update"]);
        assert!(apt.last_update.lock().unwrap().is_none());

        // A failed check isn't reused; the next request retries the update.
        assert!(apt.get_updates().await.is_err());
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["sudo apt-get update", "sudo apt-get update"]
        );
    }
}
