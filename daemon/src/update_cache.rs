//! Background refresh of the package-manager update list, so no request path runs
//! `sudo apt-get update` (or `brew update`).

use crate::package_manager::PackageManager;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

/// Result of the most recent background update check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    /// The first check since startup has not finished yet.
    #[default]
    Pending,
    Ready(Vec<String>),
    Failed(String),
}

/// Latest update check result, shared between the refresh task and request handlers.
#[derive(Debug, Clone, Default)]
pub struct UpdateCache {
    status: Arc<RwLock<UpdateStatus>>,
    refresh: Arc<Notify>,
}

impl UpdateCache {
    pub fn status(&self) -> UpdateStatus {
        self.status
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set(&self, status: UpdateStatus) {
        *self.status.write().unwrap_or_else(|e| e.into_inner()) = status;
    }

    /// Asks the refresh task to run its next check now instead of waiting for the interval.
    pub fn request_refresh(&self) {
        self.refresh.notify_one();
    }

    #[cfg(test)]
    pub async fn refresh_requested(&self) {
        self.refresh.notified().await;
    }

    /// Checks for updates now, then again every `interval` and whenever a refresh is
    /// requested. Requested checks don't move the interval schedule, so the cache TTL
    /// in `get_updates` still expires on time.
    pub async fn run_refresh_loop(self, pm: Arc<Box<dyn PackageManager>>, interval: Duration) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = self.refresh.notified() => info!("update check requested"),
            }
            self.refresh_once(pm.as_ref().as_ref()).await;
        }
    }

    async fn refresh_once(&self, pm: &dyn PackageManager) {
        let status = match pm.get_updates().await {
            Ok(updates) => UpdateStatus::Ready(updates),
            Err(e) => {
                warn!("background update check failed: {e}");
                UpdateStatus::Failed(e.to_string())
            }
        };
        self.set(status);
    }
}

/// Refresh interval for `package_manager.update_interval_mins`.
pub fn refresh_interval(update_interval_mins: u64) -> Duration {
    Duration::from_secs(update_interval_mins.max(1).saturating_mul(60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingPm {
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl PackageManager for CountingPm {
        fn name(&self) -> &str {
            "counting"
        }
        fn version(&self) -> String {
            "1".to_string()
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn get_updates(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail {
                return Err("apt exploded".into());
            }
            Ok(vec![format!("pkg{n}")])
        }
        async fn dry_run_upgrade(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }
        async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }
        fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }
        fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            unreachable!()
        }
    }

    fn counting_pm(fail: bool) -> (Arc<Box<dyn PackageManager>>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let pm: Box<dyn PackageManager> = Box::new(CountingPm {
            calls: calls.clone(),
            fail,
        });
        (Arc::new(pm), calls)
    }

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_refresh_loop_runs_at_startup_and_on_interval() {
        let cache = UpdateCache::default();
        assert_eq!(cache.status(), UpdateStatus::Pending);
        let (pm, calls) = counting_pm(false);
        tokio::spawn(cache.clone().run_refresh_loop(pm, Duration::from_secs(60)));

        settle().await;
        assert_eq!(cache.status(), UpdateStatus::Ready(vec!["pkg1".into()]));

        tokio::time::advance(Duration::from_secs(59)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(cache.status(), UpdateStatus::Ready(vec!["pkg2".into()]));
    }

    #[tokio::test(start_paused = true)]
    async fn test_request_refresh_runs_check_early() {
        let cache = UpdateCache::default();
        let (pm, calls) = counting_pm(false);
        tokio::spawn(
            cache
                .clone()
                .run_refresh_loop(pm, Duration::from_secs(3600)),
        );
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        cache.request_refresh();
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.status(), UpdateStatus::Ready(vec!["pkg2".into()]));
    }

    #[tokio::test(start_paused = true)]
    async fn test_requested_check_keeps_interval_schedule() {
        let cache = UpdateCache::default();
        let (pm, calls) = counting_pm(false);
        tokio::spawn(cache.clone().run_refresh_loop(pm, Duration::from_secs(60)));
        settle().await;

        tokio::time::advance(Duration::from_secs(50)).await;
        cache.request_refresh();
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // The scheduled check still runs 60 s after startup, not 60 s after the request.
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn test_failed_check_is_reported() {
        let cache = UpdateCache::default();
        let (pm, _) = counting_pm(true);
        tokio::spawn(cache.clone().run_refresh_loop(pm, Duration::from_secs(60)));
        settle().await;
        assert_eq!(cache.status(), UpdateStatus::Failed("apt exploded".into()));
    }

    #[test]
    fn test_refresh_interval() {
        assert_eq!(refresh_interval(360), Duration::from_secs(360 * 60));
        assert_eq!(refresh_interval(0), Duration::from_secs(60));
        assert_eq!(refresh_interval(u64::MAX), Duration::from_secs(u64::MAX));
    }
}
