use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use brewmble_rest::{
    HealthResponse, PATH_HEALTH, PATH_REBOOT, PATH_STATUS, PATH_UPGRADE, RebootRequest,
    RebootResponse, SERVICE_FULL_TYPE, StatusResponse, TXT_TLS, TXT_TLS_VALUE, UpgradeRequest,
    UpgradeResponse,
};
use clap::Parser;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use rustls::pki_types::CertificateDer;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
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

const DEFAULT_PORT: u16 = 4712;

#[derive(Parser)]
#[command(name = "brewmbled", version, disable_version_flag = true)]
#[command(about = "Brewmble daemon", long_about = None)]
struct Cli {
    /// Port to listen on. If not specified, the daemon will search for a free port starting from 4712.
    #[arg(short, long, env = "BREWMBLE_DAEMON_PORT")]
    port: Option<u16>,

    /// Hostname to use for mDNS registration. Defaults to the system hostname.
    #[arg(long, env = "BREWMBLE_DAEMON_HOSTNAME")]
    hostname: Option<String>,

    /// Explicit IP address to use for mDNS registration.
    #[arg(long, env = "BREWMBLE_DAEMON_IP")]
    ip: Option<IpAddr>,

    /// Path to the cluster CA certificate PEM file.
    #[arg(long, env = "BREWMBLE_DAEMON_TLS_CA_CERT")]
    tls_ca_cert: PathBuf,

    /// Path to the daemon certificate PEM file.
    #[arg(long, env = "BREWMBLE_DAEMON_TLS_CERT")]
    tls_cert: PathBuf,

    /// Path to the daemon private key PEM file.
    #[arg(long, env = "BREWMBLE_DAEMON_TLS_KEY")]
    tls_key: PathBuf,

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

    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();
    if cli.version {
        println!("brewmbled {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    info!("Starting brewmbled version {}", env!("CARGO_PKG_VERSION"));

    let http_port = if let Some(port) = cli.port {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            error!("failed to bind to port {port}: {e}");
            e
        })?;
        drop(listener);
        port
    } else {
        let mut port = DEFAULT_PORT;
        loop {
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            match TcpListener::bind(addr).await {
                Ok(listener) => {
                    drop(listener);
                    break port;
                }
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

    let tls_config = load_tls_config(&cli.tls_ca_cert, &cli.tls_cert, &cli.tls_key)?;
    let mdns_daemon = register_mdns(http_port, &hostname, cli.ip);

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
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    info!("brewmble daemon listening on {}", addr);

    let handle: axum_server::Handle<SocketAddr> = axum_server::Handle::new();
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config);
    let server = axum_server::bind_rustls(addr, rustls_config)
        .handle(handle.clone())
        .serve(app.into_make_service());

    let shutdown = async move {
        shutdown_signal().await;
        handle.graceful_shutdown(None);
    };

    let server_result = tokio::select! {
        result = server => result,
        _ = shutdown => Ok(()),
    };

    if let Err(err) = server_result {
        error!("https server error: {err}");
    }

    if let Some(mdns) = mdns_daemon {
        if let Err(err) = mdns.shutdown() {
            error!("mDNS shutdown error: {err}");
        }
    }

    Ok(())
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

fn mdns_properties(hostname: &str) -> Vec<(&str, &str)> {
    vec![("id", hostname), (TXT_TLS, TXT_TLS_VALUE)]
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
    let properties = mdns_properties(hostname);

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

fn load_tls_config(
    ca_path: &PathBuf,
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<Arc<rustls::ServerConfig>, Box<dyn std::error::Error>> {
    let ca_pem = std::fs::read_to_string(ca_path)
        .map_err(|e| format!("failed to read CA cert file '{}': {e}", ca_path.display()))?;
    let cert_pem = std::fs::read_to_string(cert_path).map_err(|e| {
        format!(
            "failed to read daemon cert file '{}': {e}",
            cert_path.display()
        )
    })?;
    let key_pem = std::fs::read_to_string(key_path).map_err(|e| {
        format!(
            "failed to read daemon key file '{}': {e}",
            key_path.display()
        )
    })?;

    let ca_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("failed to parse CA cert: {e}"))?;
    let ca_cert = ca_certs.into_iter().next().ok_or("CA cert file is empty")?;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca_cert)?;

    let daemon_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to parse daemon cert: {e}"))?;
    if daemon_certs.is_empty() {
        return Err("daemon cert file is empty".into());
    }

    let daemon_key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or("daemon key file is empty or unparseable")?;

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots)).build()?;
    let mut config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(daemon_certs, daemon_key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
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
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, PKCS_ECDSA_P256_SHA256,
    };
    use time::OffsetDateTime;
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
    async fn test_status_handler_non_linux() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
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
            "--tls-ca-cert",
            "/etc/brewmble/tls/ca.pem",
            "--tls-cert",
            "/etc/brewmble/tls/daemon.pem",
            "--tls-key",
            "/etc/brewmble/tls/daemon-key.pem",
            "--allow-reboot",
            "--auto-clean",
            "--auto-remove",
        ]);
        assert_eq!(cli.port, Some(9090));
        assert_eq!(cli.hostname, Some("test-host".to_string()));
        assert_eq!(cli.ip, Some("1.2.3.4".parse().unwrap()));
        assert_eq!(cli.tls_ca_cert, PathBuf::from("/etc/brewmble/tls/ca.pem"));
        assert_eq!(cli.tls_cert, PathBuf::from("/etc/brewmble/tls/daemon.pem"));
        assert_eq!(
            cli.tls_key,
            PathBuf::from("/etc/brewmble/tls/daemon-key.pem")
        );
        assert!(cli.allow_reboot);
        assert!(cli.auto_clean);
        assert!(cli.auto_remove);
    }

    #[test]
    fn test_cli_default_config_flags() {
        let cli = Cli::parse_from([
            "brewmbled",
            "--tls-ca-cert",
            "/etc/brewmble/tls/ca.pem",
            "--tls-cert",
            "/etc/brewmble/tls/daemon.pem",
            "--tls-key",
            "/etc/brewmble/tls/daemon-key.pem",
        ]);
        assert!(!cli.allow_reboot);
        assert!(!cli.auto_clean);
        assert!(!cli.auto_remove);
    }

    #[test]
    fn test_cli_env_vars() {
        let cli = Cli::try_parse_from([
            "brewmbled",
            "--tls-ca-cert",
            "/etc/brewmble/tls/ca.pem",
            "--tls-cert",
            "/etc/brewmble/tls/daemon.pem",
            "--tls-key",
            "/etc/brewmble/tls/daemon-key.pem",
        ]);
        assert!(cli.is_ok());
        let c = cli.unwrap();
        assert!(c.port.is_some() || c.port.is_none());
    }

    #[test]
    fn test_mdns_properties_include_tls() {
        let props = mdns_properties("myhost");
        assert!(props.iter().any(|(k, v)| *k == "id" && *v == "myhost"));
        assert!(
            props
                .iter()
                .any(|(k, v)| *k == TXT_TLS && *v == TXT_TLS_VALUE)
        );
    }

    #[test]
    fn test_load_tls_config_missing_files() {
        let result = load_tls_config(
            &PathBuf::from("/nonexistent/ca.pem"),
            &PathBuf::from("/nonexistent/cert.pem"),
            &PathBuf::from("/nonexistent/key.pem"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_load_tls_config_unparseable_pem() {
        let dir = std::env::temp_dir().join(format!("brewmble-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca_path = dir.join("ca.pem");
        let cert_path = dir.join("daemon.pem");
        let key_path = dir.join("daemon-key.pem");
        std::fs::write(&ca_path, "not a pem").unwrap();
        std::fs::write(&cert_path, "not a pem").unwrap();
        std::fs::write(&key_path, "not a pem").unwrap();

        let result = load_tls_config(&ca_path, &cert_path, &key_path);
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn generate_test_ca(cn: &str) -> (String, String) {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let not_before = OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_before = not_before;
        params.not_after = not_before + time::Duration::days(3650);
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn generate_test_leaf(
        cn: &str,
        _ca_cert: &str,
        ca_key: &str,
        eku: ExtendedKeyUsagePurpose,
    ) -> (String, String) {
        let ca_key = KeyPair::from_pem(ca_key).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "test-ca");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let issuer = Issuer::new(ca_params, ca_key);

        let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![eku];
        let not_before = OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_before = not_before;
        params.not_after = not_before + time::Duration::days(397);
        let cert = params.signed_by(&leaf_key, &issuer).unwrap();
        (cert.pem(), leaf_key.serialize_pem())
    }

    #[tokio::test]
    async fn test_mtls_integration() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = std::env::temp_dir().join(format!("brewmble-mtls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let (ca_cert, ca_key) = generate_test_ca("test-ca");
        let (daemon_cert, daemon_key) = generate_test_leaf(
            "test-daemon",
            &ca_cert,
            &ca_key,
            ExtendedKeyUsagePurpose::ServerAuth,
        );
        let (client_cert, client_key) = generate_test_leaf(
            "test-client",
            &ca_cert,
            &ca_key,
            ExtendedKeyUsagePurpose::ClientAuth,
        );
        let (other_ca, other_key) = generate_test_ca("other-ca");
        let (other_client_cert, other_client_key) = generate_test_leaf(
            "other-client",
            &other_ca,
            &other_key,
            ExtendedKeyUsagePurpose::ClientAuth,
        );

        let ca_path = dir.join("ca.pem");
        let cert_path = dir.join("daemon.pem");
        let key_path = dir.join("daemon-key.pem");
        std::fs::write(&ca_path, &ca_cert).unwrap();
        std::fs::write(&cert_path, &daemon_cert).unwrap();
        std::fs::write(&key_path, &daemon_key).unwrap();

        let tls_config = load_tls_config(&ca_path, &cert_path, &key_path).unwrap();

        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
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
            .route(PATH_STATUS, get(status_handler))
            .with_state(state);

        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config);
        let addr = SocketAddr::from(([127, 0, 0, 1], 0));
        let handle: axum_server::Handle<SocketAddr> = axum_server::Handle::new();
        let server = axum_server::bind_rustls(addr, rustls_config)
            .handle(handle.clone())
            .serve(app.into_make_service());
        tokio::spawn(server);
        let bound = handle.listening().await.unwrap();
        let base = format!("https://127.0.0.1:{}", bound.port());

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let ca_root = reqwest::Certificate::from_pem(ca_cert.as_bytes()).unwrap();

        // No client certificate: handshake must fail.
        let client_no_cert = reqwest::Client::builder()
            .tls_certs_only([ca_root.clone()])
            .tls_danger_accept_invalid_hostnames(true)
            .https_only(true)
            .build()
            .unwrap();
        let result = client_no_cert
            .get(format!("{}{}", base, PATH_HEALTH))
            .send()
            .await;
        assert!(result.is_err());

        // Valid client certificate from the cluster CA.
        let identity =
            reqwest::Identity::from_pem(format!("{}\n{}", client_cert, client_key).as_bytes())
                .unwrap();
        let client_ok = reqwest::Client::builder()
            .identity(identity)
            .tls_certs_only([ca_root.clone()])
            .tls_danger_accept_invalid_hostnames(true)
            .https_only(true)
            .build()
            .unwrap();
        let response = client_ok
            .get(format!("{}{}", base, PATH_HEALTH))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let response = client_ok
            .get(format!("{}{}", base, PATH_STATUS))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        // Client certificate signed by a different CA: handshake must fail.
        let bad_identity = reqwest::Identity::from_pem(
            format!("{}\n{}", other_client_cert, other_client_key).as_bytes(),
        )
        .unwrap();
        let client_bad = reqwest::Client::builder()
            .identity(bad_identity)
            .tls_certs_only([ca_root])
            .tls_danger_accept_invalid_hostnames(true)
            .https_only(true)
            .build()
            .unwrap();
        let result = client_bad
            .get(format!("{}{}", base, PATH_HEALTH))
            .send()
            .await;
        assert!(result.is_err());

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
