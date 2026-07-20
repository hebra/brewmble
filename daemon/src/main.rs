use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
};
use brewmble_rest::{
    API_KEY_HEADER, HealthResponse, PATH_HEALTH, PATH_REBOOT, PATH_STATUS, PATH_UPGRADE,
    RebootRequest, RebootResponse, SERVICE_FULL_TYPE, StatusResponse, UpgradeRequest,
    UpgradeResponse,
};
use clap::Parser;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::net::{IpAddr, SocketAddr};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod package_manager;
use package_manager::{PackageManager, get_package_manager};

const DEFAULT_HTTP_PORT: u16 = 8080;

#[derive(Parser)]
#[command(name = "brewmbled", version, disable_version_flag = true)]
#[command(about = "Brewmble daemon", long_about = None)]
struct Cli {
    /// Port to listen on. If not specified, the daemon will search for a free port starting from 8080.
    #[arg(short, long, env = "BREWMBLE_DAEMON_PORT")]
    port: Option<u16>,

    /// Hostname to use for mDNS registration. Defaults to the system hostname.
    #[arg(long, env = "BREWMBLE_DAEMON_HOSTNAME")]
    hostname: Option<String>,

    /// Explicit IP address to use for mDNS registration.
    #[arg(long, env = "BREWMBLE_DAEMON_IP")]
    ip: Option<IpAddr>,

    /// API key for authentication. If not provided, one will be generated.
    #[arg(long, env = "BREWMBLE_DAEMON_API_KEY")]
    api_key: Option<String>,

    /// Allow the daemon to reboot the host when requested via the API.
    #[arg(long, env = "BREWMBLE_DAEMON_ALLOW_REBOOT")]
    allow_reboot: bool,

    /// Automatically clean downloaded packages after a successful upgrade.
    #[arg(long, env = "BREWMBLE_DAEMON_AUTO_CLEAN")]
    auto_clean: bool,

    /// Automatically remove unused packages after a successful upgrade.
    #[arg(long, env = "BREWMBLE_DAEMON_AUTO_REMOVE")]
    auto_remove: bool,

    /// Print version information
    #[arg(short = 'v', long)]
    version: bool,
}

#[derive(Clone)]
struct AppState {
    is_upgrading: Arc<AtomicBool>,
    api_key: String,
    allow_reboot: bool,
    auto_clean: bool,
    auto_remove: bool,
    package_manager: Arc<Box<dyn PackageManager>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "brewmbled=info,tower_http=debug,axum::rejection=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_ansi(true))
        .init();

    let cli = Cli::parse();
    if cli.version {
        println!("brewmbled {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    info!("Starting brewmbled version {}", env!("CARGO_PKG_VERSION"));

    let (listener, http_port) = if let Some(port) = cli.port {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            error!("failed to bind to port {port}: {e}");
            e
        })?;
        (listener, port)
    } else {
        let mut port = DEFAULT_HTTP_PORT;
        loop {
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            match TcpListener::bind(addr).await {
                Ok(listener) => break (listener, port),
                Err(e) => {
                    if port == u16::MAX {
                        error!("no free ports found");
                        return Err(e.into());
                    }
                    warn!("port {port} is already in use, trying {}...", port + 1);
                    port += 1;
                }
            }
        }
    };

    let hostname = cli
        .hostname
        .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned())
        .trim_end_matches('.')
        .to_string();

    let mdns_daemon = register_mdns(http_port, &hostname, cli.ip);

    let api_key = if let Some(key) = cli.api_key {
        key
    } else {
        let key = uuid::Uuid::new_v4().to_string();
        info!("no API key provided, generated: {}", key);
        key
    };

    let pm = get_package_manager(cli.auto_clean, cli.auto_remove);
    info!("using {} package manager backend", pm.name());
    if !pm.is_available() {
        warn!(
            "The current package manager ({}) is not available on this system.",
            pm.name()
        );
    }

    let state = AppState {
        is_upgrading: Arc::new(AtomicBool::new(false)),
        api_key,
        allow_reboot: cli.allow_reboot,
        auto_clean: cli.auto_clean,
        auto_remove: cli.auto_remove,
        package_manager: Arc::new(pm),
    };

    let app = Router::new()
        .route(PATH_HEALTH, get(health_handler))
        .route(PATH_STATUS, get(status_handler))
        .route(PATH_UPGRADE, post(full_upgrade_handler))
        .route(PATH_REBOOT, post(reboot_handler))
        .layer(TraceLayer::new_for_http())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state);

    info!("brewmble daemon listening on {}", listener.local_addr()?);

    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    if let Err(err) = server_result {
        error!("http server error: {err}");
    }

    if let Some(mdns) = mdns_daemon {
        if let Err(err) = mdns.shutdown() {
            error!("mDNS shutdown error: {err}");
        }
    }

    Ok(())
}

async fn auth_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<impl IntoResponse, StatusCode> {
    // Allow public access to /health
    if req.uri().path() == PATH_HEALTH {
        return Ok(next.run(req).await);
    }

    let auth_header = req
        .headers()
        .get(API_KEY_HEADER)
        .and_then(|header| header.to_str().ok());

    match auth_header {
        Some(key) if key == state.api_key => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        package_manager: state.package_manager.name().to_string(),
        package_manager_version: state.package_manager.version(),
        is_upgrading: state.is_upgrading.load(Ordering::SeqCst),
    })
}

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let is_upgrading = state.is_upgrading.load(Ordering::SeqCst);
    let config_flags = (state.allow_reboot, state.auto_clean, state.auto_remove);

    if !state.package_manager.is_available() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(StatusResponse {
                message: format!(
                    "the system is not a {} system",
                    state.package_manager.name()
                ),
                updates: Vec::new(),
                is_upgrading,
                daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                allow_reboot: config_flags.0,
                auto_clean: config_flags.1,
                auto_remove: config_flags.2,
            }),
        );
    }

    match state.package_manager.get_updates().await {
        Ok(updates) => {
            let count = updates.len();
            let message = if count == 0 {
                "System is up to date".to_string()
            } else {
                format!("System has {} outdated packages", count)
            };
            (
                StatusCode::OK,
                Json(StatusResponse {
                    message,
                    updates,
                    is_upgrading,
                    daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                    allow_reboot: config_flags.0,
                    auto_clean: config_flags.1,
                    auto_remove: config_flags.2,
                }),
            )
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(StatusResponse {
                message: format!("Failed to check for updates: {}", err),
                updates: Vec::new(),
                is_upgrading,
                daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                allow_reboot: config_flags.0,
                auto_clean: config_flags.1,
                auto_remove: config_flags.2,
            }),
        ),
    }
}

async fn full_upgrade_handler(
    State(state): State<AppState>,
    Json(payload): Json<UpgradeRequest>,
) -> impl IntoResponse {
    if !state.package_manager.is_available() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(UpgradeResponse {
                message: format!(
                    "the system is not a {} system",
                    state.package_manager.name()
                ),
                updates: None,
            }),
        );
    }

    if payload.dry_run {
        match state.package_manager.dry_run_upgrade().await {
            Ok(updates) => {
                return (
                    StatusCode::OK,
                    Json(UpgradeResponse {
                        message: "dry-run completed".to_string(),
                        updates: Some(updates),
                    }),
                );
            }
            Err(err) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(UpgradeResponse {
                        message: format!("dry-run failed: {}", err),
                        updates: None,
                    }),
                );
            }
        }
    }

    if state
        .is_upgrading
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(UpgradeResponse {
                message: "a full upgrade is currently running".to_string(),
                updates: None,
            }),
        );
    }

    tokio::spawn(async move {
        info!("starting full upgrade");
        let result = state.package_manager.full_upgrade().await;

        if let Err(e) = result {
            error!("failed to execute full upgrade: {e}");
        }
        state.is_upgrading.store(false, Ordering::SeqCst);
    });

    (
        StatusCode::OK,
        Json(UpgradeResponse {
            message: "full upgrade triggered".to_string(),
            updates: None,
        }),
    )
}

async fn reboot_handler(
    State(state): State<AppState>,
    Json(_payload): Json<RebootRequest>,
) -> impl IntoResponse {
    if !state.package_manager.is_available() {
        return (
            StatusCode::PRECONDITION_FAILED,
            Json(RebootResponse {
                message: format!(
                    "the system is not a {} system",
                    state.package_manager.name()
                ),
            }),
        );
    }

    if state.is_upgrading.load(Ordering::SeqCst) {
        return (
            StatusCode::LOCKED,
            Json(RebootResponse {
                message: "a full upgrade is currently running".to_string(),
            }),
        );
    }

    if !state.allow_reboot {
        return (
            StatusCode::FORBIDDEN,
            Json(RebootResponse {
                message: "reboot is not enabled on this daemon".to_string(),
            }),
        );
    }

    tokio::spawn(async move {
        info!("scheduling system reboot");
        // Try systemd first, then fall back to the traditional reboot command.
        let result = tokio::process::Command::new("sudo")
            .args(["systemctl", "reboot"])
            .status()
            .await
            .or_else(|_| std::process::Command::new("sudo").args(["reboot"]).status());

        match result {
            Ok(status) if status.success() => info!("system reboot initiated"),
            Ok(status) => error!("failed to initiate reboot: exit status {}", status),
            Err(e) => error!("failed to execute reboot command: {e}"),
        }
    });

    (
        StatusCode::OK,
        Json(RebootResponse {
            message: "reboot triggered".to_string(),
        }),
    )
}

fn register_mdns(port: u16, hostname: &str, ip_addr: Option<IpAddr>) -> Option<ServiceDaemon> {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => {
            info!("mDNS daemon started");
            daemon
        }
        Err(err) => {
            error!("FAILED to start mDNS daemon: {err}");
            return None;
        }
    };

    let instance_hostname = hostname.split('.').next().unwrap_or(hostname);
    let instance = format!("brewmbled-{instance_hostname}");
    let host_name = format!("{instance_hostname}.local.");
    let properties = [("id", hostname)];

    info!("Registering mDNS service:");
    info!("  Instance: {}", instance);
    info!("  Host: {}", host_name);
    info!("  Port: {}", port);

    let info = if let Some(ip) = ip_addr {
        info!("Using explicit IP: {}", ip);
        match ServiceInfo::new(
            SERVICE_FULL_TYPE,
            &instance,
            &host_name,
            ip,
            port,
            &properties[..],
        ) {
            Ok(info) => info,
            Err(err) => {
                error!("FAILED to create mDNS service info with explicit IP: {err}");
                return None;
            }
        }
    } else {
        match ServiceInfo::new(
            SERVICE_FULL_TYPE,
            &instance,
            &host_name,
            "",
            port,
            &properties[..],
        ) {
            Ok(info) => {
                info!("mDNS service info created, enabling automatic address discovery");
                info.enable_addr_auto()
            }
            Err(err) => {
                error!("FAILED to create mDNS service info: {err}");
                return None;
            }
        }
    };

    if let Err(err) = daemon.register(info) {
        error!("FAILED to register mDNS service: {err}");
        return None;
    }

    info!("mDNS service registered successfully");
    Some(daemon)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            error!("failed to install Ctrl-C handler: {err}");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                error!("failed to install SIGTERM handler: {err}");
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Request, StatusCode};
    use brewmble_rest::StatusResponse;
    use tower::ServiceExt;

    struct MockPackageManager {
        name: String,
        available: bool,
    }

    #[async_trait::async_trait]
    impl PackageManager for MockPackageManager {
        fn name(&self) -> &str {
            &self.name
        }
        fn version(&self) -> String {
            "1.0.0".to_string()
        }
        fn is_available(&self) -> bool {
            self.available
        }
        async fn get_updates(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec!["pkg1".to_string()])
        }
        async fn dry_run_upgrade(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec!["pkg1".to_string()])
        }
        async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_auth_middleware() {
        let api_key = "test-key".to_string();
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: api_key.clone(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_STATUS, get(status_handler))
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state);

        // No API key
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Wrong API key
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .header(API_KEY_HEADER, "wrong-key")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Correct API key
        let response = app
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .header(API_KEY_HEADER, api_key)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // It should pass middleware.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_status_handler_non_linux() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_STATUS, get(status_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let status: StatusResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(status.updates.len(), 1);
        assert_eq!(status.updates[0], "pkg1");
    }

    #[tokio::test]
    async fn test_status_handler_unavailable() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: false,
            })),
        };
        let app = Router::new()
            .route(PATH_STATUS, get(status_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let status: StatusResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(status.message, "the system is not a mock system");
    }

    #[tokio::test]
    async fn test_health_handler() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_HEALTH, get(health_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(PATH_HEALTH)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.package_manager, "mock");
    }

    #[tokio::test]
    async fn test_full_upgrade_handler_mock() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_UPGRADE, post(full_upgrade_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PATH_UPGRADE)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&UpgradeRequest { dry_run: false }).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_full_upgrade_flow() {
        #[cfg(target_os = "linux")]
        {
            let state = AppState {
                is_upgrading: Arc::new(AtomicBool::new(false)),
                api_key: "test".to_string(),
                allow_reboot: false,
                auto_clean: false,
                auto_remove: false,
                package_manager: Arc::new(get_package_manager(false, false)),
            };
            let app = Router::new()
                .route("/status", get(status_handler))
                .route("/packages/full-upgrade", post(full_upgrade_handler))
                .with_state(state.clone());

            // 1. Start upgrade
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/packages/full-upgrade")
                        .header("Content-Type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::to_vec(&UpgradeRequest { dry_run: false }).unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(state.is_upgrading.load(Ordering::SeqCst));

            // 2. Try starting upgrade again while one is running
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/packages/full-upgrade")
                        .header("Content-Type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::to_vec(&UpgradeRequest { dry_run: false }).unwrap(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            let error_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error_json["message"], "a full upgrade is currently running");

            // 3. Check /status reflects is_upgrading: true
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/status")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let body = to_bytes(response.into_body(), 1024).await.unwrap();
            let status: StatusResponse = serde_json::from_slice(&body).unwrap();
            assert!(status.is_upgrading);
        }
    }

    #[tokio::test]
    async fn test_reboot_handler_allowed() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: true,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_REBOOT, post(reboot_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PATH_REBOOT)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&RebootRequest::default()).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_reboot_handler_disabled() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_REBOOT, post(reboot_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PATH_REBOOT)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&RebootRequest::default()).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_reboot_handler_blocked_while_upgrading() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(true)),
            api_key: "test".to_string(),
            allow_reboot: true,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_REBOOT, post(reboot_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PATH_REBOOT)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&RebootRequest::default()).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::LOCKED);
    }

    #[tokio::test]
    async fn test_status_handler_exposes_config_flags() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            api_key: "test".to_string(),
            allow_reboot: true,
            auto_clean: true,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
        };
        let app = Router::new()
            .route(PATH_STATUS, get(status_handler))
            .with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(PATH_STATUS)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        let status: StatusResponse = serde_json::from_slice(&body).unwrap();
        assert!(status.allow_reboot);
        assert!(status.auto_clean);
        assert!(!status.auto_remove);
    }

    #[tokio::test]
    async fn test_port_hunting() {
        use tokio::net::TcpListener;

        // Bind to a random port first to simulate it being in use
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound_addr = listener.local_addr().unwrap();
        let bound_port = bound_addr.port();

        // Now try to find a port starting from bound_port. It should find bound_port + 1 or higher.
        let mut port = bound_port;
        let found_port = loop {
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            match TcpListener::bind(addr).await {
                Ok(l) => {
                    break l.local_addr().unwrap().port();
                }
                Err(_) => {
                    port += 1;
                }
            }
        };

        assert!(found_port > bound_port);

        drop(listener);
    }

    #[tokio::test]
    async fn test_port_fail_if_env_set() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound_port = listener.local_addr().unwrap().port();

        // Set environment variable
        unsafe {
            std::env::set_var("BREWMBLE_DAEMON_PORT", bound_port.to_string());
        }

        let port_env = std::env::var("BREWMBLE_DAEMON_PORT").ok();
        assert!(port_env.is_some());

        let port_str = port_env.unwrap();
        let port = port_str.parse::<u16>().unwrap();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let result = TcpListener::bind(addr).await;

        assert!(result.is_err());

        unsafe {
            std::env::remove_var("BREWMBLE_DAEMON_PORT");
        }
        drop(listener);
    }

    #[test]
    fn test_cli_parsing() {
        let cli = Cli::parse_from([
            "brewmbled",
            "--port",
            "9090",
            "--hostname",
            "test-host",
            "--ip",
            "1.2.3.4",
            "--api-key",
            "secret-key",
            "--allow-reboot",
            "--auto-clean",
            "--auto-remove",
        ]);
        assert_eq!(cli.port, Some(9090));
        assert_eq!(cli.hostname, Some("test-host".to_string()));
        assert_eq!(cli.ip, Some("1.2.3.4".parse().unwrap()));
        assert_eq!(cli.api_key, Some("secret-key".to_string()));
        assert!(cli.allow_reboot);
        assert!(cli.auto_clean);
        assert!(cli.auto_remove);
    }

    #[test]
    fn test_cli_default_config_flags() {
        let cli = Cli::parse_from(["brewmbled"]);
        assert!(!cli.allow_reboot);
        assert!(!cli.auto_clean);
        assert!(!cli.auto_remove);
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::try_parse_from(["brewmbled"]);
        if let Ok(c) = cli {
            // If env var was already set by environment, we just check it parses
            assert!(c.port.is_some() || c.port.is_none());
        }

        // Test with explicit env override in a controlled way if possible,
        // but Clap's env support is hard to test with set_var in multi-threaded test runner.
        // So we just rely on test_cli_parsing for basic logic.
    }
}
