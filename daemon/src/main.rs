use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::{get, post},
};
use brewmble_rest::{
    HealthResponse, PATH_HEALTH, PATH_REBOOT, PATH_STATUS, PATH_UPGRADE, RebootRequest,
    RebootResponse, SERVICE_FULL_TYPE, StatusResponse, TXT_TLS, TXT_TLS_VALUE, UpgradeRequest,
    UpgradeResponse,
};
use clap::{Parser, Subcommand};
use mdns_sd::{ServiceDaemon, ServiceInfo};
use rustls::pki_types::CertificateDer;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::task::JoinHandle;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod client_auth;
pub mod config;
mod package_manager;
mod update_cache;
use package_manager::{PackageManager, get_package_manager};
use update_cache::{UpdateCache, UpdateStatus};

const DEFAULT_PORT: u16 = 4712;

fn server_bind_ip(config: &config::DaemonConfig) -> IpAddr {
    config
        .server
        .bind_address
        .unwrap_or_else(|| "0.0.0.0".parse().unwrap())
}

/// Binds the HTTPS listener that is later handed to the server, so the port
/// advertised via mDNS is the one actually held. A configured port must be
/// free; without one, ports from `start` upwards are tried while they are in use.
fn bind_listener(
    bind_ip: IpAddr,
    configured_port: Option<u16>,
    start: u16,
) -> std::io::Result<std::net::TcpListener> {
    let listener = match configured_port {
        Some(port) => std::net::TcpListener::bind((bind_ip, port))?,
        None => {
            let mut port = start;
            loop {
                match std::net::TcpListener::bind((bind_ip, port)) {
                    Ok(listener) => break listener,
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && port < u16::MAX => {
                        warn!("port {port} is already in use, trying {}...", port + 1);
                        port += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[derive(Parser, Debug)]
#[command(name = "brewmbled", version, disable_version_flag = true)]
#[command(about = "Brewmble daemon", long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short = 'c', long, global = true)]
    config: Option<PathBuf>,

    /// Print version information
    #[arg(short = 'v', long)]
    version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum Commands {
    /// Manage daemon configuration
    Config {
        #[command(subcommand)]
        subcommand: ConfigCommands,
    },
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
enum ConfigCommands {
    /// Generate a default configuration file template
    Init {
        /// Overwrite existing configuration file if it exists
        #[arg(long)]
        force: bool,
    },
    /// Display the resolved active configuration
    Show,
    /// Open the configuration file in $EDITOR
    Edit,
    /// Validate configuration syntax and check TLS certificates
    Validate,
}

fn run_config_init(path: Option<&Path>, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    let target_path = path
        .map(PathBuf::from)
        .unwrap_or_else(config::default_config_path);
    config::init_config_file(&target_path, force)?;
    println!(
        "Configuration template written to {}",
        target_path.display()
    );
    Ok(())
}

fn run_config_show(path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = config::resolve_config_path(path)?;
    let cfg = config::load_config(&config_path)?;
    let yaml = serde_yaml_ng::to_string(&cfg)?;
    print!("{yaml}");
    Ok(())
}

fn run_config_edit(path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let target_path = match path {
        Some(p) => p.to_path_buf(),
        None => match config::resolve_config_path(None) {
            Ok(p) => p,
            Err(_) => config::default_config_path(),
        },
    };

    if !target_path.exists() {
        config::init_config_file(&target_path, false)?;
    }

    let editor = std::env::var("EDITOR").unwrap_or_default();
    let (program, extra_args) = editor_command(&editor)?;
    let status = std::process::Command::new(&program)
        .args(&extra_args)
        .arg(&target_path)
        .status()
        .map_err(|e| format!("Failed to launch editor '{program}': {e}"))?;

    if !status.success() {
        return Err(format!("Editor '{program}' exited with status: {status}").into());
    }

    Ok(())
}

/// Splits `$EDITOR` into a program and its arguments. A path (containing `/`) to an
/// existing file is used as-is, so unquoted paths with spaces work; anything else is split with
/// shell quoting rules (`"/Applications/My Editor/bin/edit" --wait`). Empty means `vi`.
fn editor_command(editor: &str) -> Result<(String, Vec<String>), String> {
    let editor = editor.trim();
    if editor.is_empty() {
        return Ok(("vi".to_string(), Vec::new()));
    }
    if editor.contains('/') && Path::new(editor).is_file() {
        return Ok((editor.to_string(), Vec::new()));
    }
    let mut parts = shlex::split(editor)
        .filter(|parts| !parts.is_empty())
        .ok_or_else(|| format!("Cannot parse $EDITOR '{editor}': unbalanced quotes"))?
        .into_iter();
    let program = parts.next().unwrap_or_default();
    Ok((program, parts.collect()))
}

fn run_config_validate(path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = config::resolve_config_path(path)?;
    let cfg = config::load_config(&config_path)?;
    let report = config::validate_config(&cfg);

    for warning in &report.warnings {
        eprintln!("Warning: {warning}");
    }

    if !report.is_valid() {
        for err in &report.errors {
            eprintln!("Error: {err}");
        }
        return Err("Configuration validation failed".into());
    }

    println!("Configuration at '{}' is valid.", config_path.display());
    Ok(())
}

fn run_config(
    subcommand: ConfigCommands,
    explicit_config: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    match subcommand {
        ConfigCommands::Init { force } => run_config_init(explicit_config, force),
        ConfigCommands::Show => run_config_show(explicit_config),
        ConfigCommands::Edit => run_config_edit(explicit_config),
        ConfigCommands::Validate => run_config_validate(explicit_config),
    }
}

#[derive(Clone)]
struct AppState {
    is_upgrading: Arc<AtomicBool>,
    allow_reboot: bool,
    auto_clean: bool,
    auto_remove: bool,
    package_manager: Arc<Box<dyn PackageManager>>,
    pm_available: bool,
    pm_version: String,
    upgrade_handle: Arc<std::sync::Mutex<Option<JoinHandle<()>>>>,
    reboot: Arc<std::sync::Mutex<RebootState>>,
    updates: UpdateCache,
    reboot_commands: &'static [RebootCommand],
}

/// Longest delay a client may request before a scheduled reboot.
const MAX_REBOOT_DELAY_SECS: u64 = 3600;

/// At most one reboot is scheduled at a time. While one is pending or running,
/// new upgrades and reboots are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RebootState {
    #[default]
    Idle,
    /// Waiting out its delay; the id lets a cancelled task recognise it was replaced.
    Pending(u64),
    Rebooting,
}

struct UpgradingGuard(Arc<AtomicBool>);

impl UpgradingGuard {
    fn new(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
}

impl Drop for UpgradingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
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

    if let Some(Commands::Config { subcommand }) = cli.command {
        return run_config(subcommand, cli.config.as_deref());
    }
    info!("Starting brewmbled version {}", env!("CARGO_PKG_VERSION"));

    let config_path = match config::resolve_config_path(cli.config.as_deref()) {
        Ok(path) => path,
        Err(err) => {
            error!("{err}");
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };

    let daemon_config = match config::load_config(&config_path) {
        Ok(cfg) => cfg,
        Err(err) => {
            error!(
                "failed to load configuration from '{}': {err}",
                config_path.display()
            );
            eprintln!(
                "error: failed to load configuration from '{}': {err}",
                config_path.display()
            );
            std::process::exit(1);
        }
    };

    if let Err(err) = config::ensure_valid_config(&daemon_config) {
        error!("configuration validation failed: {err}");
        eprintln!("error: {err}");
        std::process::exit(1);
    }

    info!("Loaded configuration from {}", config_path.display());

    let allowed_clients = config::allowed_client_fingerprints(&daemon_config.tls)?;
    client_auth::log_allow_list("active", &daemon_config.tls, &allowed_clients);
    let client_access = client_auth::ClientAccess::new(allowed_clients);
    #[cfg(unix)]
    client_auth::spawn_reload_on_sighup(config_path.clone(), client_access.clone());

    let bind_ip = server_bind_ip(&daemon_config);
    let listener =
        bind_listener(bind_ip, daemon_config.server.port, DEFAULT_PORT).map_err(|e| {
            error!("failed to bind HTTPS listener on {bind_ip}: {e}");
            e
        })?;
    let addr = listener.local_addr()?;
    let http_port = addr.port();

    let hostname = daemon_config
        .server
        .hostname
        .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned())
        .trim_end_matches('.')
        .to_string();

    let tls_config = load_tls_config(
        &daemon_config.tls.ca_cert,
        &daemon_config.tls.cert,
        &daemon_config.tls.key,
    )?;
    let mdns_daemon = if daemon_config.server.mdns {
        register_mdns(http_port, &hostname, daemon_config.server.ip)
    } else {
        info!("mDNS disabled by configuration");
        None
    };

    let pm = get_package_manager(
        daemon_config.features.auto_clean,
        daemon_config.features.auto_remove,
        daemon_config.package_manager.update_interval_mins,
    );
    let pm_name = pm.name().to_string();
    let pm_available = pm.is_available();
    let pm_version = pm.version();
    info!("using {} package manager backend", pm_name);
    if !pm_available {
        warn!(
            "The current package manager ({}) is not available on this system.",
            pm_name
        );
    }

    let package_manager: Arc<Box<dyn PackageManager>> = Arc::new(pm);
    let updates = UpdateCache::default();
    if pm_available {
        tokio::spawn(updates.clone().run_refresh_loop(
            package_manager.clone(),
            update_cache::refresh_interval(daemon_config.package_manager.update_interval_mins),
        ));
    }

    let state = AppState {
        is_upgrading: Arc::new(AtomicBool::new(false)),
        allow_reboot: daemon_config.features.allow_reboot,
        auto_clean: daemon_config.features.auto_clean,
        auto_remove: daemon_config.features.auto_remove,
        package_manager,
        pm_available,
        pm_version,
        upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
        reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
        updates,
        reboot_commands: reboot_commands(),
    };
    let shutdown_state = state.clone();

    // A 408 only drops the request future; package-manager commands already
    // running in `spawn_blocking` (and `sudo apt-get` children) keep going.
    // `/status` only reads the background update check; dry-run apt checks
    // run as a shared task that later requests join.
    let app = build_router(state, client_access).layer((
        TraceLayer::new_for_http(),
        TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(300),
        ),
    ));

    info!("brewmble daemon listening on {}", addr);

    let handle: axum_server::Handle<SocketAddr> = axum_server::Handle::new();
    let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config);
    let server = axum_server::from_tcp_rustls(listener, rustls_config)?
        .map(client_auth::ClientIdentityAcceptor::new)
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

    if let Ok(mut guard) = shutdown_state.upgrade_handle.lock() {
        if let Some(handle) = guard.take() {
            let abort_handle = handle.abort_handle();
            let timeout = tokio::time::Duration::from_secs(30);
            match tokio::time::timeout(timeout, handle).await {
                Ok(Ok(())) => info!("upgrade task finished cleanly"),
                Ok(Err(e)) => error!("upgrade task panicked or was cancelled: {e}"),
                Err(_) => {
                    abort_handle.abort();
                    error!("upgrade task did not finish in time; aborted");
                }
            }
        }
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
        package_manager_version: state.pm_version.clone(),
        is_upgrading: state.is_upgrading.load(Ordering::SeqCst),
    })
}

async fn status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let is_upgrading = state.is_upgrading.load(Ordering::SeqCst);
    let config_flags = (state.allow_reboot, state.auto_clean, state.auto_remove);

    if !state.pm_available {
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
                checking: false,
            }),
        );
    }

    match state.updates.status() {
        UpdateStatus::Pending => (
            StatusCode::OK,
            Json(StatusResponse {
                message: "First update check is still running".to_string(),
                updates: Vec::new(),
                is_upgrading,
                daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                allow_reboot: config_flags.0,
                auto_clean: config_flags.1,
                auto_remove: config_flags.2,
                checking: true,
            }),
        ),
        UpdateStatus::Ready(updates) => {
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
                    checking: false,
                }),
            )
        }
        UpdateStatus::Failed(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(StatusResponse {
                message: format!("Failed to check for updates: {}", err),
                updates: Vec::new(),
                is_upgrading,
                daemon_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                allow_reboot: config_flags.0,
                auto_clean: config_flags.1,
                auto_remove: config_flags.2,
                checking: false,
            }),
        ),
    }
}

async fn full_upgrade_handler(
    State(state): State<AppState>,
    Json(payload): Json<UpgradeRequest>,
) -> impl IntoResponse {
    if !state.pm_available {
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

    {
        // Hold the reboot state while claiming the upgrade lock, so a pending reboot
        // always wins against an upgrade that arrives as its delay expires.
        let reboot = lock_reboot(&state.reboot);
        if *reboot != RebootState::Idle {
            return (
                StatusCode::LOCKED,
                Json(UpgradeResponse {
                    message: "a reboot is scheduled; cancel it before upgrading".to_string(),
                    updates: None,
                }),
            );
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
    }

    let handle = tokio::spawn(async move {
        let _guard = UpgradingGuard::new(state.is_upgrading.clone());
        info!("starting full upgrade");
        let result = state.package_manager.full_upgrade().await;

        if let Err(e) = result {
            error!("failed to execute full upgrade: {e}");
        }
        state.updates.request_refresh();
    });

    if let Ok(mut guard) = state.upgrade_handle.lock() {
        *guard = Some(handle);
    }

    (
        StatusCode::OK,
        Json(UpgradeResponse {
            message: "full upgrade triggered".to_string(),
            updates: None,
        }),
    )
}

#[async_trait::async_trait]
trait RebootRunner {
    async fn run(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus>;
}

struct TokioRebootRunner;

#[async_trait::async_trait]
impl RebootRunner for TokioRebootRunner {
    async fn run(&self, program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
        tokio::process::Command::new(program)
            .args(args)
            .status()
            .await
    }
}

fn lock_reboot(reboot: &std::sync::Mutex<RebootState>) -> std::sync::MutexGuard<'_, RebootState> {
    reboot.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits out the delay, then reboots only if this reboot is still the scheduled
/// one and no upgrade is running.
async fn run_scheduled_reboot<R: RebootRunner + Send + Sync>(
    id: u64,
    delay_secs: u64,
    reboot: Arc<std::sync::Mutex<RebootState>>,
    is_upgrading: Arc<AtomicBool>,
    runner: &R,
    commands: &[RebootCommand],
) {
    if delay_secs > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
    }
    {
        let mut current = lock_reboot(&reboot);
        if *current != RebootState::Pending(id) {
            info!("scheduled reboot was cancelled");
            return;
        }
        if is_upgrading.load(Ordering::SeqCst) {
            warn!("skipping scheduled reboot: a full upgrade is running");
            *current = RebootState::Idle;
            return;
        }
        *current = RebootState::Rebooting;
    }
    if !execute_reboot(runner, commands).await {
        *lock_reboot(&reboot) = RebootState::Idle;
    }
}

/// A reboot command: program and arguments, run as given (sudoers rules match them exactly).
type RebootCommand = (&'static str, &'static [&'static str]);

const LINUX_REBOOT_COMMANDS: &[RebootCommand] = &[
    ("/usr/bin/sudo", &["-n", "/usr/bin/systemctl", "reboot"]),
    ("/usr/bin/sudo", &["-n", "/usr/sbin/reboot"]),
];

const MACOS_REBOOT_COMMANDS: &[RebootCommand] =
    &[("/usr/bin/sudo", &["-n", "/sbin/shutdown", "-r", "now"])];

/// Reboot commands for this platform, tried in order. Empty where reboot is unsupported.
fn reboot_commands() -> &'static [RebootCommand] {
    if cfg!(target_os = "linux") {
        LINUX_REBOOT_COMMANDS
    } else if cfg!(target_os = "macos") {
        MACOS_REBOOT_COMMANDS
    } else {
        &[]
    }
}

/// Tries each reboot command in turn and returns whether one succeeded.
async fn execute_reboot<R: RebootRunner + Send + Sync>(
    runner: &R,
    commands: &[RebootCommand],
) -> bool {
    for (program, args) in commands {
        let command = args.join(" ");
        match runner.run(program, args).await {
            Ok(status) if status.success() => {
                info!("system reboot initiated via `{command}`");
                return true;
            }
            Ok(status) => error!("`{command}` failed: exit status {status}"),
            Err(e) => error!("failed to execute `{command}`: {e}"),
        }
    }
    error!("failed to initiate reboot");
    false
}

async fn reboot_handler(
    State(state): State<AppState>,
    Json(payload): Json<RebootRequest>,
) -> impl IntoResponse {
    if state.reboot_commands.is_empty() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(RebootResponse {
                message: "reboot is not supported on this platform".to_string(),
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

    let delay = payload.delay_secs.unwrap_or(0);
    if delay > MAX_REBOOT_DELAY_SECS {
        return (
            StatusCode::BAD_REQUEST,
            Json(RebootResponse {
                message: format!("delay_secs must be at most {MAX_REBOOT_DELAY_SECS}"),
            }),
        );
    }

    let id = {
        let mut current = lock_reboot(&state.reboot);
        if *current != RebootState::Idle {
            return (
                StatusCode::CONFLICT,
                Json(RebootResponse {
                    message: "a reboot is already scheduled".to_string(),
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
        let id = NEXT_REBOOT_ID.fetch_add(1, Ordering::SeqCst);
        *current = RebootState::Pending(id);
        id
    };

    let reboot = state.reboot.clone();
    let is_upgrading = state.is_upgrading.clone();
    let commands = state.reboot_commands;
    tokio::spawn(async move {
        info!("scheduling system reboot in {delay}s");
        run_scheduled_reboot(
            id,
            delay,
            reboot,
            is_upgrading,
            &TokioRebootRunner,
            commands,
        )
        .await;
    });

    (
        StatusCode::OK,
        Json(RebootResponse {
            message: if delay > 0 {
                format!("reboot scheduled in {delay}s")
            } else {
                "reboot triggered".to_string()
            },
        }),
    )
}

static NEXT_REBOOT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

async fn cancel_reboot_handler(State(state): State<AppState>) -> impl IntoResponse {
    let mut current = lock_reboot(&state.reboot);
    let (status, message) = match *current {
        RebootState::Pending(_) => {
            *current = RebootState::Idle;
            info!("scheduled reboot cancelled");
            (StatusCode::OK, "reboot cancelled")
        }
        RebootState::Rebooting => (StatusCode::CONFLICT, "the reboot is already in progress"),
        RebootState::Idle => (StatusCode::NOT_FOUND, "no reboot is scheduled"),
    };
    (
        status,
        Json(RebootResponse {
            message: message.to_string(),
        }),
    )
}

fn mdns_properties(hostname: &str) -> Vec<(&str, &str)> {
    vec![("id", hostname), (TXT_TLS, TXT_TLS_VALUE)]
}

fn mdns_host_name(hostname: &str) -> String {
    let instance_hostname = hostname.split('.').next().unwrap_or(hostname);
    format!("brewmbled-{instance_hostname}.local.")
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
    let host_name = mdns_host_name(hostname);
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

/// API routes, each guarded by the client-certificate role it requires.
fn build_router(state: AppState, access: client_auth::ClientAccess) -> Router {
    let read_routes = Router::new()
        .route(PATH_HEALTH, get(health_handler))
        .route(PATH_STATUS, get(status_handler))
        .route_layer(from_fn_with_state(
            access.clone(),
            client_auth::require_read,
        ));
    let operator_routes = Router::new()
        .route(PATH_UPGRADE, post(full_upgrade_handler))
        .route(
            PATH_REBOOT,
            post(reboot_handler).delete(cancel_reboot_handler),
        )
        .route_layer(from_fn_with_state(access, client_auth::require_operator));
    read_routes.merge(operator_routes).with_state(state)
}

pub(crate) fn load_tls_config(
    ca_path: &Path,
    cert_path: &Path,
    key_path: &Path,
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

    #[test]
    fn test_editor_command_parsing() {
        let cmd = |editor: &str| editor_command(editor).unwrap();
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        assert_eq!(cmd(""), ("vi".to_string(), vec![]));
        assert_eq!(cmd("  "), ("vi".to_string(), vec![]));
        assert_eq!(cmd("nano"), ("nano".to_string(), vec![]));
        assert_eq!(cmd("code --wait"), ("code".to_string(), args(&["--wait"])));
        assert_eq!(
            cmd(r#""/opt/My Editor/bin/edit" --wait -n"#),
            (
                "/opt/My Editor/bin/edit".to_string(),
                args(&["--wait", "-n"])
            )
        );
        assert_eq!(
            cmd(r"/opt/My\ Editor/edit"),
            ("/opt/My Editor/edit".to_string(), vec![])
        );
        assert!(editor_command(r#""/opt/My Editor"#).is_err());
    }

    #[test]
    fn test_editor_command_existing_path_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let editor = dir.path().join("My Editor");
        std::fs::write(&editor, "").unwrap();
        let editor = editor.to_str().unwrap();
        assert_eq!(
            editor_command(editor).unwrap(),
            (editor.to_string(), vec![])
        );
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        };
        state
            .updates
            .set(UpdateStatus::Ready(vec!["pkg1".to_string()]));
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
            pm_available: false,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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
    #[ignore = "runs a real package upgrade on the host"]
    async fn test_full_upgrade_flow() {
        #[cfg(target_os = "linux")]
        {
            let state = AppState {
                is_upgrading: Arc::new(AtomicBool::new(false)),
                allow_reboot: false,
                auto_clean: false,
                auto_remove: false,
                package_manager: Arc::new(get_package_manager(false, false, 0)),
                pm_available: true,
                pm_version: "test".to_string(),
                upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
                reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
                updates: UpdateCache::default(),
                reboot_commands: reboot_commands(),
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

    struct PanickingMockPackageManager;

    #[async_trait::async_trait]
    impl PackageManager for PanickingMockPackageManager {
        fn name(&self) -> &str {
            "panic-mock"
        }
        fn version(&self) -> String {
            "1.0.0".to_string()
        }
        fn is_available(&self) -> bool {
            true
        }
        async fn get_updates(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![])
        }
        async fn dry_run_upgrade(
            &self,
        ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
            Ok(vec![])
        }
        async fn full_upgrade(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            panic!("simulated upgrade panic");
        }
        fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_full_upgrade_guard_resets_on_panic() {
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(PanickingMockPackageManager)),
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        };
        let app = Router::new()
            .route(PATH_UPGRADE, post(full_upgrade_handler))
            .with_state(state.clone());

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

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            if !state.is_upgrading.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        assert!(!state.is_upgrading.load(Ordering::SeqCst));
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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

    struct MockRebootRunner {
        calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
        responses: std::sync::Mutex<Vec<std::io::Result<std::process::ExitStatus>>>,
    }

    impl MockRebootRunner {
        fn new(responses: Vec<std::io::Result<std::process::ExitStatus>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                responses: std::sync::Mutex::new(responses),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl RebootRunner for MockRebootRunner {
        async fn run(
            &self,
            program: &str,
            args: &[&str],
        ) -> std::io::Result<std::process::ExitStatus> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            self.responses.lock().unwrap().remove(0)
        }
    }

    fn exit_success() -> std::process::ExitStatus {
        std::process::Command::new("true").status().unwrap()
    }

    fn exit_failure() -> std::process::ExitStatus {
        std::process::Command::new("false").status().unwrap()
    }

    #[tokio::test]
    async fn test_execute_reboot_uses_sudo_n() {
        let runner = MockRebootRunner::new(vec![Ok(exit_success())]);
        execute_reboot(&runner, LINUX_REBOOT_COMMANDS).await;
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "/usr/bin/sudo");
        assert_eq!(calls[0].1, vec!["-n", "/usr/bin/systemctl", "reboot"]);
    }

    #[tokio::test]
    async fn test_execute_reboot_falls_back_on_nonzero() {
        let runner = MockRebootRunner::new(vec![Ok(exit_failure()), Ok(exit_success())]);
        execute_reboot(&runner, LINUX_REBOOT_COMMANDS).await;
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["-n", "/usr/bin/systemctl", "reboot"]);
        assert_eq!(calls[1].1, vec!["-n", "/usr/sbin/reboot"]);
    }

    #[tokio::test]
    async fn test_execute_reboot_falls_back_on_spawn_error() {
        let runner = MockRebootRunner::new(vec![
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "not found",
            )),
            Ok(exit_success()),
        ]);
        execute_reboot(&runner, LINUX_REBOOT_COMMANDS).await;
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1, vec!["-n", "/usr/sbin/reboot"]);
    }

    #[tokio::test]
    async fn test_execute_reboot_macos_uses_shutdown() {
        let runner = MockRebootRunner::new(vec![Ok(exit_success())]);
        assert!(execute_reboot(&runner, MACOS_REBOOT_COMMANDS).await);
        assert_eq!(
            runner.calls(),
            vec![(
                "/usr/bin/sudo".to_string(),
                vec!["-n", "/sbin/shutdown", "-r", "now"]
                    .into_iter()
                    .map(String::from)
                    .collect::<Vec<_>>()
            )]
        );

        let runner = MockRebootRunner::new(vec![Ok(exit_failure())]);
        assert!(!execute_reboot(&runner, MACOS_REBOOT_COMMANDS).await);
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn test_reboot_commands_match_platform() {
        if cfg!(target_os = "linux") {
            assert_eq!(reboot_commands(), LINUX_REBOOT_COMMANDS);
        } else if cfg!(target_os = "macos") {
            assert_eq!(reboot_commands(), MACOS_REBOOT_COMMANDS);
        } else {
            assert!(reboot_commands().is_empty());
        }
    }

    fn reboot_state(state: RebootState) -> Arc<std::sync::Mutex<RebootState>> {
        Arc::new(std::sync::Mutex::new(state))
    }

    #[tokio::test]
    async fn test_scheduled_reboot_runs_when_still_pending() {
        let runner = MockRebootRunner::new(vec![Ok(exit_success())]);
        let reboot = reboot_state(RebootState::Pending(7));
        let upgrading = Arc::new(AtomicBool::new(false));
        run_scheduled_reboot(
            7,
            0,
            reboot.clone(),
            upgrading.clone(),
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(*reboot.lock().unwrap(), RebootState::Rebooting);
        assert!(!upgrading.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_scheduled_reboot_skipped_after_cancel() {
        let runner = MockRebootRunner::new(vec![]);
        let reboot = reboot_state(RebootState::Idle);
        run_scheduled_reboot(
            7,
            0,
            reboot.clone(),
            Arc::new(AtomicBool::new(false)),
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert!(runner.calls().is_empty());
        assert_eq!(*reboot.lock().unwrap(), RebootState::Idle);
    }

    #[tokio::test]
    async fn test_scheduled_reboot_skipped_when_replaced() {
        let runner = MockRebootRunner::new(vec![]);
        let reboot = reboot_state(RebootState::Pending(8));
        run_scheduled_reboot(
            7,
            0,
            reboot.clone(),
            Arc::new(AtomicBool::new(false)),
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert!(runner.calls().is_empty());
        assert_eq!(*reboot.lock().unwrap(), RebootState::Pending(8));
    }

    #[tokio::test]
    async fn test_scheduled_reboot_rechecks_upgrade_lock_after_delay() {
        let runner = MockRebootRunner::new(vec![]);
        let reboot = reboot_state(RebootState::Pending(7));
        let upgrading = Arc::new(AtomicBool::new(true));
        run_scheduled_reboot(
            7,
            0,
            reboot.clone(),
            upgrading,
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert!(runner.calls().is_empty());
        assert_eq!(*reboot.lock().unwrap(), RebootState::Idle);
    }

    #[tokio::test]
    async fn test_scheduled_reboot_failure_clears_state() {
        let runner = MockRebootRunner::new(vec![Ok(exit_failure()), Ok(exit_failure())]);
        let reboot = reboot_state(RebootState::Pending(7));
        run_scheduled_reboot(
            7,
            0,
            reboot.clone(),
            Arc::new(AtomicBool::new(false)),
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert_eq!(runner.calls().len(), 2);
        assert_eq!(*reboot.lock().unwrap(), RebootState::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn test_scheduled_reboot_waits_for_delay() {
        let runner = Arc::new(MockRebootRunner::new(vec![Ok(exit_success())]));
        let reboot = reboot_state(RebootState::Pending(7));
        let task = {
            let runner = runner.clone();
            let reboot = reboot.clone();
            tokio::spawn(async move {
                run_scheduled_reboot(
                    7,
                    60,
                    reboot,
                    Arc::new(AtomicBool::new(false)),
                    &*runner,
                    LINUX_REBOOT_COMMANDS,
                )
                .await
            })
        };
        tokio::time::sleep(std::time::Duration::from_secs(59)).await;
        assert!(runner.calls().is_empty());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        task.await.unwrap();
        assert_eq!(runner.calls().len(), 1);
    }

    fn reboot_test_state(allow_reboot: bool) -> AppState {
        AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            allow_reboot,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        }
    }

    fn reboot_app(state: AppState) -> Router {
        Router::new()
            .route(
                PATH_REBOOT,
                post(reboot_handler).delete(cancel_reboot_handler),
            )
            .route(PATH_UPGRADE, post(full_upgrade_handler))
            .with_state(state)
    }

    async fn send(app: &Router, method: &str, uri: &str, body: &str) -> StatusCode {
        app.clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("Content-Type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_reboot_handler_unsupported_platform() {
        let mut state = reboot_test_state(true);
        state.reboot_commands = &[];
        let app = reboot_app(state.clone());
        assert_eq!(
            send(&app, "POST", PATH_REBOOT, r#"{"delay_secs":60}"#).await,
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(*state.reboot.lock().unwrap(), RebootState::Idle);
    }

    #[tokio::test]
    async fn test_reboot_handler_does_not_need_package_manager() {
        let mut state = reboot_test_state(true);
        state.pm_available = false;
        let app = reboot_app(state.clone());
        assert_eq!(
            send(&app, "POST", PATH_REBOOT, r#"{"delay_secs":3600}"#).await,
            StatusCode::OK
        );
        assert_eq!(send(&app, "DELETE", PATH_REBOOT, "").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_reboot_handler_rejects_long_delay() {
        let state = reboot_test_state(true);
        let app = reboot_app(state.clone());
        let body = format!("{{\"delay_secs\":{}}}", MAX_REBOOT_DELAY_SECS + 1);
        assert_eq!(
            send(&app, "POST", PATH_REBOOT, &body).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(*state.reboot.lock().unwrap(), RebootState::Idle);
    }

    #[tokio::test]
    async fn test_reboot_pending_blocks_second_reboot_and_upgrade_until_cancelled() {
        let state = reboot_test_state(true);
        let app = reboot_app(state.clone());
        let body = format!("{{\"delay_secs\":{MAX_REBOOT_DELAY_SECS}}}");
        assert_eq!(send(&app, "POST", PATH_REBOOT, &body).await, StatusCode::OK);
        assert!(matches!(
            *state.reboot.lock().unwrap(),
            RebootState::Pending(_)
        ));

        assert_eq!(
            send(&app, "POST", PATH_REBOOT, "{}").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            send(&app, "POST", PATH_UPGRADE, r#"{"dry_run":false}"#).await,
            StatusCode::LOCKED
        );
        assert!(!state.is_upgrading.load(Ordering::SeqCst));

        assert_eq!(send(&app, "DELETE", PATH_REBOOT, "").await, StatusCode::OK);
        assert_eq!(*state.reboot.lock().unwrap(), RebootState::Idle);
        assert_eq!(
            send(&app, "DELETE", PATH_REBOOT, "").await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn test_pending_reboot_wins_over_upgrade_at_expiry() {
        let state = reboot_test_state(true);
        *state.reboot.lock().unwrap() = RebootState::Pending(7);
        let app = reboot_app(state.clone());
        // Upgrade admission is refused without touching the upgrade lock...
        assert_eq!(
            send(&app, "POST", PATH_UPGRADE, r#"{"dry_run":false}"#).await,
            StatusCode::LOCKED
        );
        assert!(!state.is_upgrading.load(Ordering::SeqCst));
        // ...so the expiring timer still reboots.
        let runner = MockRebootRunner::new(vec![Ok(exit_success())]);
        run_scheduled_reboot(
            7,
            0,
            state.reboot.clone(),
            state.is_upgrading.clone(),
            &runner,
            LINUX_REBOOT_COMMANDS,
        )
        .await;
        assert_eq!(runner.calls().len(), 1);
        assert_eq!(*state.reboot.lock().unwrap(), RebootState::Rebooting);
    }

    #[tokio::test]
    async fn test_cancel_reboot_in_progress_conflicts() {
        let state = reboot_test_state(true);
        *state.reboot.lock().unwrap() = RebootState::Rebooting;
        let app = reboot_app(state.clone());
        assert_eq!(
            send(&app, "DELETE", PATH_REBOOT, "").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            send(&app, "POST", PATH_UPGRADE, r#"{"dry_run":false}"#).await,
            StatusCode::LOCKED
        );
    }

    async fn status_for(update_status: Option<UpdateStatus>) -> (StatusCode, StatusResponse) {
        let state = router_test_state();
        if let Some(update_status) = update_status {
            state.updates.set(update_status);
        }
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
        let code = response.status();
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        (code, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn test_status_handler_reads_background_check_only() {
        // The mock package manager would report "pkg1"; /status must not call it.
        let (code, status) = status_for(None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(status.updates.is_empty());
        assert!(status.checking);
        assert_eq!(status.message, "First update check is still running");

        let (code, status) = status_for(Some(UpdateStatus::Ready(vec![]))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(status.message, "System is up to date");
        assert!(!status.checking);

        let (code, status) =
            status_for(Some(UpdateStatus::Ready(vec!["a".into(), "b".into()]))).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(status.updates, vec!["a", "b"]);
        assert_eq!(status.message, "System has 2 outdated packages");

        let (code, status) = status_for(Some(UpdateStatus::Failed("boom".into()))).await;
        assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(status.message, "Failed to check for updates: boom");
    }

    #[tokio::test]
    async fn test_full_upgrade_requests_update_refresh() {
        let state = router_test_state();
        let (status, _) = full_upgrade_handler(
            State(state.clone()),
            Json(UpgradeRequest { dry_run: false }),
        )
        .await
        .into_response()
        .into_parts();
        assert_eq!(status.status, StatusCode::OK);
        let handle = state.upgrade_handle.lock().unwrap().take().unwrap();
        handle.await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.updates.refresh_requested(),
        )
        .await
        .expect("upgrade should request an update check");
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
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

    const LOOPBACK: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    #[test]
    fn test_bind_listener_hunts_past_ports_in_use_and_keeps_listener() {
        let occupied = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let listener = bind_listener(LOOPBACK, None, occupied_port).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port > occupied_port);

        // The returned listener still holds the port; nothing else can take it.
        let err = std::net::TcpListener::bind((LOOPBACK, port)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn test_bind_listener_configured_port_in_use_fails() {
        let occupied = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        let occupied_port = occupied.local_addr().unwrap().port();

        let err = bind_listener(LOOPBACK, Some(occupied_port), DEFAULT_PORT).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn test_bind_listener_configured_port_is_bound() {
        let probe = std::net::TcpListener::bind((LOOPBACK, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let listener = bind_listener(LOOPBACK, Some(port), DEFAULT_PORT).unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    #[test]
    fn test_bind_listener_hunting_stops_on_other_errors() {
        // 192.0.2.1 (TEST-NET-1) is not assigned locally, so bind fails with
        // AddrNotAvailable; hunting must not walk through the port range.
        let unassigned: IpAddr = "192.0.2.1".parse().unwrap();
        let err = bind_listener(unassigned, None, DEFAULT_PORT).unwrap_err();
        assert_ne!(err.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn test_cli_parsing() {
        let cli = Cli::parse_from(["brewmbled", "-c", "/etc/brewmble/brewmbled.yaml"]);
        assert_eq!(
            cli.config,
            Some(PathBuf::from("/etc/brewmble/brewmbled.yaml"))
        );
        assert!(!cli.version);
        assert!(cli.command.is_none());

        let cli = Cli::parse_from(["brewmbled", "-v"]);
        assert!(cli.version);

        let cli = Cli::parse_from(["brewmbled"]);
        assert_eq!(cli.config, None);
        assert!(!cli.version);
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_daemon_startup_from_config_file() {
        let dir =
            std::env::temp_dir().join(format!("brewmble-startup-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let (ca_cert, ca_key) = generate_test_ca("test-ca");
        let (daemon_cert, daemon_key) = generate_test_leaf(
            "test-daemon",
            &ca_cert,
            &ca_key,
            ExtendedKeyUsagePurpose::ServerAuth,
        );

        let ca_path = dir.join("ca.pem");
        let cert_path = dir.join("daemon.pem");
        let key_path = dir.join("daemon-key.pem");
        std::fs::write(&ca_path, &ca_cert).unwrap();
        std::fs::write(&cert_path, &daemon_cert).unwrap();
        std::fs::write(&key_path, &daemon_key).unwrap();

        let config_file = dir.join("brewmbled.yaml");
        let cfg = config::DaemonConfig {
            server: config::ServerConfig {
                port: Some(9090),
                hostname: Some("node-startup".to_string()),
                ip: None,
                bind_address: None,
                mdns: true,
            },
            tls: config::TlsConfig {
                ca_cert: ca_path,
                cert: cert_path,
                key: key_path,
                allowed_clients: None,
            },
            features: config::FeaturesConfig {
                allow_reboot: true,
                auto_clean: true,
                auto_remove: true,
            },
            package_manager: config::PackageManagerConfig {
                update_interval_mins: 180,
            },
        };
        config::save_config(&config_file, &cfg).unwrap();

        // Test loading and state init
        let loaded = config::load_config(&config_file).unwrap();
        assert!(config::ensure_valid_config(&loaded).is_ok());

        let tls_config = load_tls_config(&loaded.tls.ca_cert, &loaded.tls.cert, &loaded.tls.key);
        assert!(tls_config.is_ok());

        let pm = get_package_manager(
            loaded.features.auto_clean,
            loaded.features.auto_remove,
            loaded.package_manager.update_interval_mins,
        );
        let state = AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            allow_reboot: loaded.features.allow_reboot,
            auto_clean: loaded.features.auto_clean,
            auto_remove: loaded.features.auto_remove,
            package_manager: Arc::new(pm),
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        };

        assert!(state.allow_reboot);
        assert!(state.auto_clean);
        assert!(state.auto_remove);

        let _ = std::fs::remove_dir_all(&dir);
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
    fn test_mdns_host_name() {
        assert_eq!(mdns_host_name("node1"), "brewmbled-node1.local.");
        assert_eq!(
            mdns_host_name("node1.example.com"),
            "brewmbled-node1.local."
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
        ca_cert: &str,
        ca_key: &str,
        eku: ExtendedKeyUsagePurpose,
    ) -> (String, String) {
        generate_test_leaf_with_role(cn, ca_cert, ca_key, eku, None)
    }

    fn generate_test_leaf_with_role(
        cn: &str,
        _ca_cert: &str,
        ca_key: &str,
        eku: ExtendedKeyUsagePurpose,
        role: Option<brewmble_rest::ClientRole>,
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
        if let Some(role) = role {
            params
                .distinguished_name
                .push(DnType::OrganizationalUnitName, role.subject_ou());
        }
        params.extended_key_usages = vec![eku];
        let not_before = OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_before = not_before;
        params.not_after = not_before + time::Duration::days(397);
        let cert = params.signed_by(&leaf_key, &issuer).unwrap();
        (cert.pem(), leaf_key.serialize_pem())
    }

    fn router_test_state() -> AppState {
        AppState {
            is_upgrading: Arc::new(AtomicBool::new(false)),
            allow_reboot: false,
            auto_clean: false,
            auto_remove: false,
            package_manager: Arc::new(Box::new(MockPackageManager {
                name: "mock".to_string(),
                available: true,
            })),
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        }
    }

    async fn route_status(
        app: &Router,
        method: &str,
        path: &str,
        identity: Option<client_auth::ClientIdentity>,
    ) -> StatusCode {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("Content-Type", "application/json");
        if let Some(identity) = identity {
            builder = builder.extension(identity);
        }
        let body = if method == "POST" {
            axum::body::Body::from("{}")
        } else {
            axum::body::Body::empty()
        };
        app.clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_router_enforces_roles_per_route() {
        use brewmble_rest::ClientRole;
        let access = client_auth::ClientAccess::default();
        let app = build_router(router_test_state(), access);
        let id = |role| client_auth::ClientIdentity {
            fingerprint: "aa".to_string(),
            role,
        };

        for path in [PATH_HEALTH, PATH_STATUS] {
            assert_eq!(
                route_status(&app, "GET", path, None).await,
                StatusCode::UNAUTHORIZED
            );
            for role in [None, Some(ClientRole::Read), Some(ClientRole::Operator)] {
                assert_eq!(
                    route_status(&app, "GET", path, Some(id(role))).await,
                    StatusCode::OK
                );
            }
        }

        for path in [PATH_UPGRADE, PATH_REBOOT] {
            assert_eq!(
                route_status(&app, "POST", path, None).await,
                StatusCode::UNAUTHORIZED
            );
            for role in [None, Some(ClientRole::Read)] {
                assert_eq!(
                    route_status(&app, "POST", path, Some(id(role))).await,
                    StatusCode::FORBIDDEN
                );
            }
        }
        // Operators reach the handlers: reboot is disabled and the upgrade body is invalid
        // (dry_run is required), so neither does anything.
        assert_eq!(
            route_status(
                &app,
                "POST",
                PATH_REBOOT,
                Some(id(Some(ClientRole::Operator)))
            )
            .await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            route_status(
                &app,
                "POST",
                PATH_UPGRADE,
                Some(id(Some(ClientRole::Operator)))
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn test_router_rejects_clients_missing_from_allow_list() {
        use brewmble_rest::ClientRole;
        let access = client_auth::ClientAccess::new(Some(std::collections::HashSet::from([
            "aa".to_string()
        ])));
        let app = build_router(router_test_state(), access.clone());
        let id = |fp: &str| client_auth::ClientIdentity {
            fingerprint: fp.to_string(),
            role: Some(ClientRole::Operator),
        };

        assert_eq!(
            route_status(&app, "GET", PATH_HEALTH, Some(id("aa"))).await,
            StatusCode::OK
        );
        assert_eq!(
            route_status(&app, "GET", PATH_HEALTH, Some(id("bb"))).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            route_status(&app, "POST", PATH_REBOOT, Some(id("bb"))).await,
            StatusCode::FORBIDDEN
        );

        access.replace(Some(std::collections::HashSet::new()));
        assert_eq!(
            route_status(&app, "GET", PATH_HEALTH, Some(id("aa"))).await,
            StatusCode::FORBIDDEN
        );
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
        let (operator_cert, operator_key) = generate_test_leaf_with_role(
            "test-operator",
            &ca_cert,
            &ca_key,
            ExtendedKeyUsagePurpose::ClientAuth,
            Some(brewmble_rest::ClientRole::Operator),
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
            pm_available: true,
            pm_version: "1.0.0".to_string(),
            upgrade_handle: Arc::new(std::sync::Mutex::new(None)),
            reboot: Arc::new(std::sync::Mutex::new(RebootState::Idle)),
            updates: UpdateCache::default(),
            reboot_commands: reboot_commands(),
        };
        let access = client_auth::ClientAccess::default();
        let app = build_router(state, access.clone());

        let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(tls_config);
        let listener = bind_listener(LOOPBACK, None, DEFAULT_PORT).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle: axum_server::Handle<SocketAddr> = axum_server::Handle::new();
        let server = axum_server::from_tcp_rustls(listener, rustls_config)
            .unwrap()
            .map(client_auth::ClientIdentityAcceptor::new)
            .handle(handle.clone())
            .serve(app.into_make_service());
        tokio::spawn(server);
        let bound = handle.listening().await.unwrap();
        assert_eq!(bound.port(), port);
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

        // A certificate without a role is read-only.
        let response = client_ok
            .post(format!("{}{}", base, PATH_REBOOT))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        let body: serde_json::Value =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert!(body["message"].as_str().unwrap().contains("role 'read'"));

        // An operator certificate passes authorization and reaches the reboot handler.
        let operator_identity =
            reqwest::Identity::from_pem(format!("{}\n{}", operator_cert, operator_key).as_bytes())
                .unwrap();
        let client_operator = reqwest::Client::builder()
            .identity(operator_identity)
            .tls_certs_only([ca_root.clone()])
            .tls_danger_accept_invalid_hostnames(true)
            .https_only(true)
            .build()
            .unwrap();
        let response = client_operator
            .post(format!("{}{}", base, PATH_REBOOT))
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        let body: serde_json::Value =
            serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(body["message"], "reboot is not enabled on this daemon");

        // Restricting the allow-list revokes the read-only certificate, including on
        // its already open connection.
        let operator_fp = {
            let der = rustls_pemfile::certs(&mut operator_cert.as_bytes())
                .next()
                .unwrap()
                .unwrap();
            client_auth::fingerprint(der.as_ref())
        };
        access.replace(Some(std::collections::HashSet::from([operator_fp])));
        let response = client_ok
            .get(format!("{}{}", base, PATH_HEALTH))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
        let response = client_operator
            .get(format!("{}{}", base, PATH_HEALTH))
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

    #[test]
    fn test_cli_config_commands_parsing() {
        let cli = Cli::try_parse_from(["brewmbled", "config", "init"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Config {
                subcommand: ConfigCommands::Init { force: false }
            })
        );

        let cli = Cli::try_parse_from(["brewmbled", "config", "init", "--force"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Config {
                subcommand: ConfigCommands::Init { force: true }
            })
        );

        let cli = Cli::try_parse_from(["brewmbled", "-c", "/custom/config.yaml", "config", "show"])
            .unwrap();
        assert_eq!(cli.config, Some(PathBuf::from("/custom/config.yaml")));
        assert_eq!(
            cli.command,
            Some(Commands::Config {
                subcommand: ConfigCommands::Show
            })
        );

        let cli = Cli::try_parse_from(["brewmbled", "config", "edit"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Config {
                subcommand: ConfigCommands::Edit
            })
        );

        let cli = Cli::try_parse_from(["brewmbled", "config", "validate"]).unwrap();
        assert_eq!(
            cli.command,
            Some(Commands::Config {
                subcommand: ConfigCommands::Validate
            })
        );
    }

    #[test]
    fn test_run_config_init_and_show_and_validate() {
        let dir =
            std::env::temp_dir().join(format!("brewmble-cfg-cmd-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let config_file = dir.join("brewmbled.yaml");

        // 1. Init
        let res = run_config_init(Some(&config_file), false);
        assert!(res.is_ok());
        assert!(config_file.exists());

        // Init again without force fails
        let res = run_config_init(Some(&config_file), false);
        assert!(res.is_err());

        // Init again with force succeeds
        let res = run_config_init(Some(&config_file), true);
        assert!(res.is_ok());

        // 2. Show
        let res = run_config_show(Some(&config_file));
        assert!(res.is_ok());

        // 3. Validate template (fails because TLS files don't exist yet)
        let res = run_config_validate(Some(&config_file));
        assert!(res.is_err());

        // Create valid certificates
        let (ca_cert, ca_key) = generate_test_ca("test-ca");
        let (daemon_cert, daemon_key) = generate_test_leaf(
            "test-daemon",
            &ca_cert,
            &ca_key,
            ExtendedKeyUsagePurpose::ServerAuth,
        );

        let ca_path = dir.join("ca.pem");
        let cert_path = dir.join("daemon.pem");
        let key_path = dir.join("daemon-key.pem");
        std::fs::write(&ca_path, &ca_cert).unwrap();
        std::fs::write(&cert_path, &daemon_cert).unwrap();
        std::fs::write(&key_path, &daemon_key).unwrap();

        let valid_config = config::DaemonConfig {
            server: config::ServerConfig {
                port: Some(4712),
                hostname: Some("node-1".to_string()),
                ip: None,
                bind_address: None,
                mdns: true,
            },
            tls: config::TlsConfig {
                ca_cert: ca_path,
                cert: cert_path,
                key: key_path,
                allowed_clients: None,
            },
            features: config::FeaturesConfig::default(),
            package_manager: config::PackageManagerConfig::default(),
        };

        config::save_config(&config_file, &valid_config).unwrap();

        // 4. Validate now succeeds
        let res = run_config_validate(Some(&config_file));
        assert!(res.is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
