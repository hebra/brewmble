use brewmble_rest::{
    PATH_REBOOT, PATH_STATUS, PATH_UPGRADE, RebootResponse, SERVICE_DOMAIN, SERVICE_TYPE,
    StatusResponse, TXT_TLS, UpgradeResponse,
};
use clap::{Parser, Subcommand, ValueEnum};
use flume::RecvTimeoutError;
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tabwriter::TabWriter;

mod ca;
mod export;
mod fleet;
mod pin;
mod progress;
mod sanitize;
use fleet::NodeReport;
use pin::{PinOutcome, check_pin};
use progress::NativeProgress;
use sanitize::sanitize_display;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Config {
    #[serde(default = "default_profile_name")]
    active_profile: String,
    #[serde(default)]
    profiles: HashMap<String, ProfileConfig>,
    // For migration from older versions
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    nodes: Vec<NodeConfig>,
}

impl Default for Config {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert(default_profile_name(), ProfileConfig::default());
        Self {
            active_profile: default_profile_name(),
            profiles,
            nodes: Vec::new(),
        }
    }
}

fn default_profile_name() -> String {
    "default".to_string()
}

#[derive(Serialize, Deserialize, Debug, Default, Clone)]
struct ProfileConfig {
    #[serde(default)]
    nodes: Vec<NodeConfig>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub(crate) struct NodeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fingerprint: Option<String>,
}

fn resolve_config_path(explicit_path: Option<PathBuf>) -> (PathBuf, bool) {
    resolve_config_path_inner(
        explicit_path,
        std::env::home_dir(),
        PathBuf::from(".brewmble.yaml"),
    )
}

fn resolve_config_path_inner(
    explicit_path: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    local_path: PathBuf,
) -> (PathBuf, bool) {
    if let Some(path) = explicit_path {
        return (path, true);
    }

    if let Some(mut home) = home_dir.clone() {
        home.push(".brewmble.yaml");
        if home.exists() {
            return (home, true);
        }
    }

    if local_path.exists() {
        eprintln!(
            "using config from current directory: {}",
            local_path.display()
        );
        return (local_path, true);
    }

    // Never auto-create a config in the current working directory.
    if let Some(mut home) = home_dir {
        home.push(".brewmble.yaml");
        (home, false)
    } else {
        (local_path, false)
    }
}

fn load_config(path: &Path) -> Result<Config, Box<dyn Error>> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = fs::read_to_string(path)?;
    let mut config: Config = serde_yaml_ng::from_str(&content)?;

    // Migration: Move old nodes to default profile
    if !config.nodes.is_empty() {
        let profile = config
            .profiles
            .entry(default_profile_name())
            .or_insert_with(ProfileConfig::default);
        profile.nodes.extend(config.nodes.drain(..));
    }

    // Sanitize node names to avoid terminal escape injection from config
    for profile in config.profiles.values_mut() {
        for node in &mut profile.nodes {
            if let Some(name) = node.name.take() {
                node.name = Some(sanitize_display(&name));
            }
        }
    }

    // Ensure active profile exists
    if !config.profiles.contains_key(&config.active_profile) {
        config
            .profiles
            .insert(config.active_profile.clone(), ProfileConfig::default());
    }

    Ok(config)
}

fn save_config(path: &Path, config: &Config) -> Result<(), Box<dyn Error>> {
    let content = serde_yaml_ng::to_string(config)?;
    write_config_atomically(path, &content)?;
    Ok(())
}

fn update_node_fingerprint(config: &mut Config, address: &str, fingerprint: &str) {
    let active = config.active_profile.clone();
    if let Some(profile) = config.profiles.get_mut(&active) {
        if let Some(node) = profile.nodes.iter_mut().find(|n| n.address == address) {
            node.fingerprint = Some(fingerprint.to_string());
        }
    }
}

fn save_learned_status_pins(
    path: &Path,
    profile_name: &str,
    learned_pins: Vec<(&NodeConfig, String)>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if learned_pins.is_empty() {
        return Ok(());
    }
    let mut latest = load_config(path)?;
    let Some(profile) = latest.profiles.get_mut(profile_name) else {
        return Ok(());
    };
    let mut updated = false;
    for (target, fingerprint) in learned_pins {
        if let Some(node) = profile
            .nodes
            .iter_mut()
            .find(|n| n.address == target.address)
            && node.fingerprint.is_none()
        {
            print_learned_pin(node, &fingerprint, json);
            node.fingerprint = Some(fingerprint);
            updated = true;
        }
    }
    if updated {
        save_config(path, &latest)?;
    }
    Ok(())
}

fn print_learned_pin(node: &NodeConfig, fingerprint: &str, json: bool) {
    if json {
        eprintln!("Pinned {} {}", pin::node_name(node), fingerprint);
    } else {
        println!("Pinned {} {}", pin::node_name(node), fingerprint);
    }
}

fn write_config_atomically(path: &Path, content: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        io::Write::write_all(&mut file, content.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn merge_nodes(config: &mut Config, discovered: Vec<(String, String)>) -> bool {
    let mut updated = false;
    let active_profile = config.active_profile.clone();
    let profile = config
        .profiles
        .entry(active_profile)
        .or_insert_with(ProfileConfig::default);

    for (addr, id) in discovered {
        let new_name = if id.is_empty() { None } else { Some(id) };

        // Try finding by name first if name is available
        let mut found_index = None;
        if let Some(ref name) = new_name {
            found_index = profile
                .nodes
                .iter()
                .position(|n| n.name.as_ref() == Some(name));
        }

        // If not found by name, try finding by address
        if found_index.is_none() {
            found_index = profile.nodes.iter().position(|n| n.address == addr);
        }

        if let Some(index) = found_index {
            let node = &mut profile.nodes[index];
            let mut node_updated = false;
            let has_hostname = reqwest::Url::parse(&resolve_url(&node.address))
                .is_ok_and(|url| url.domain().is_some());
            if node.address != addr && !has_hostname {
                let name = pin::node_name(node).to_string();
                node.address = addr;
                if node.fingerprint.take().is_some() {
                    eprintln!(
                        "warning: address changed for {}; cleared stored fingerprint — re-pinning required",
                        name
                    );
                }
                node_updated = true;
            }
            if node.name != new_name {
                node.name = new_name;
                node_updated = true;
            }
            if node_updated {
                updated = true;
            }
        } else {
            profile.nodes.push(NodeConfig {
                name: new_name,
                address: addr,
                fingerprint: None,
            });
            updated = true;
        }
    }
    updated
}

pub(crate) fn get_default_timeout() -> Duration {
    std::env::var("BREWMBLE_TIMEOUT")
        .ok()
        .and_then(|v| {
            v.parse::<u64>()
                .map(Duration::from_secs)
                .ok()
                .or_else(|| humantime::parse_duration(&v).ok())
        })
        .unwrap_or(Duration::from_secs(60))
}

#[derive(Parser)]
#[command(name = "brewmble", version, disable_version_flag = true)]
#[command(about = "A CLI tool for brewmble", long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, env = "BREWMBLE_CONFIG")]
    config: Option<PathBuf>,

    /// Print version information
    #[arg(short = 'v', long = "version")]
    version: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover brewmble daemons on the local network
    Discover {
        /// Time to wait for responses in seconds
        #[arg(short, long, default_value = "5", env = "BREWMBLE_TIMEOUT")]
        timeout: u64,

        /// Create and/or update a config file with newly found daemons
        #[arg(short = 'u', long = "update-config")]
        update_config: bool,
    },
    /// Show status of brewmble daemons
    Status {
        /// Get status for all discovered brewmble daemons
        #[arg(short, long)]
        all: bool,

        /// Output per-node results as JSON
        #[arg(long)]
        json: bool,

        /// Targets (host:port)
        targets: Vec<String>,
    },
    /// Manage brewmble nodes
    #[command(visible_alias = "packages")]
    Node {
        /// Perform a full system upgrade
        #[arg(short = 'u', long)]
        full_upgrade: bool,

        /// Reboot the node
        #[arg(short = 'r', long)]
        reboot: bool,

        /// Show what would be upgraded without executing
        #[arg(long, conflicts_with = "reboot", requires = "full_upgrade")]
        dry_run: bool,

        /// List node names configured in the active profile
        #[arg(short = 'l', long, conflicts_with_all = ["full_upgrade", "reboot", "dry_run", "repin"])]
        list: bool,

        /// Re-pin a node after its certificate has changed
        #[arg(long, conflicts_with_all = ["full_upgrade", "reboot", "dry_run", "list"])]
        repin: Option<String>,

        /// Output upgrade or reboot results as JSON
        #[arg(long, conflicts_with_all = ["list", "repin"])]
        json: bool,

        /// Node name(s) configured in the active profile
        #[arg(num_args = 0..)]
        nodes: Vec<String>,
    },
    /// Manage configuration profiles
    Profile {
        #[command(subcommand)]
        subcommand: ProfileCommands,
    },
    /// Initialize and manage the cluster CA
    Ca {
        #[command(subcommand)]
        subcommand: CaCommands,
    },
    /// Issue certificates for daemons or this CLI
    Cert {
        #[command(subcommand)]
        subcommand: CertCommands,
    },
    /// Export profiles, certificate pins, and credentials to an encrypted age file
    Export {
        /// Destination .age file
        #[arg(short, long)]
        output: PathBuf,
        /// Overwrite an existing export file
        #[arg(long)]
        force: bool,
    },
    /// Import profiles, certificate pins, and credentials from an encrypted age file
    Import {
        /// Source .age file
        #[arg(short, long)]
        input: PathBuf,
        /// Replace existing configuration and credentials
        #[arg(long)]
        force: bool,
        /// Validate and summarize the import without writing any state
        #[arg(long)]
        dry_run: bool,
    },
    /// Show version information for CLI and daemons
    Version {
        /// Output per-node results as JSON
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CaCommands {
    /// Create the cluster CA in the keyring
    Init {
        /// Overwrite an existing CA
        #[arg(long)]
        force: bool,
    },
    /// Show the cluster CA certificate and fingerprint
    Show,
    /// Migrate CA and client credentials from a legacy keyring store
    Migrate {
        /// Report what would be migrated without writing
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(ValueEnum, Clone)]
enum CertKind {
    Daemon,
    Client,
}

#[derive(Subcommand)]
enum CertCommands {
    /// Issue a certificate
    Issue {
        /// Kind of certificate to issue
        #[arg(short, long, value_enum)]
        kind: CertKind,
        /// Common name for the certificate (required for daemon certs)
        #[arg(short, long)]
        cn: Option<String>,
        /// Output directory for certificate files (required for daemon certs; for client
        /// certs, writes the files there instead of replacing this CLI's keyring certificate)
        #[arg(short, long)]
        out_dir: Option<PathBuf>,
        /// Role of a client certificate: `read` (health/status) or `operator` (also
        /// upgrade/reboot). Defaults to `operator`
        #[arg(short, long, value_enum)]
        role: Option<CertRole>,
        /// Overwrite existing client certificate or output files
        #[arg(long)]
        force: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CertRole {
    Read,
    Operator,
}

impl From<CertRole> for brewmble_rest::ClientRole {
    fn from(role: CertRole) -> Self {
        match role {
            CertRole::Read => brewmble_rest::ClientRole::Read,
            CertRole::Operator => brewmble_rest::ClientRole::Operator,
        }
    }
}

#[derive(Subcommand)]
enum ProfileCommands {
    /// List all profiles
    List,
    /// Set the active profile
    Use { name: String },
    /// Create a new profile
    Create { name: String },
    /// Delete a profile
    Delete { name: String },
    /// Edit the configuration file
    Edit,
}

fn main() -> Result<ExitCode, Box<dyn Error>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    if cli.version {
        println!("brewmble {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    let (config_path, config_exists) = resolve_config_path(cli.config);
    if let Some(Commands::Import {
        input,
        force,
        dry_run,
    }) = &cli.command
    {
        export::run_import(&ca::KeyringStore, input, &config_path, *force, *dry_run)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut config = match load_config(&config_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: failed to load config: {err}");
            return Ok(ExitCode::FAILURE);
        }
    };

    match cli.command {
        Some(Commands::Discover {
            timeout,
            update_config,
        }) => run_discover(Duration::from_secs(timeout), update_config, &config_path)?,
        Some(Commands::Status { all, json, targets }) => {
            if !json && targets.is_empty() && !all && !config_exists {
                println!("No config file was found or set.");
            }
            let discovery = all.then_some(|| discover_targets_with_timeout(get_default_timeout()));
            return run_status(discovery, targets, &config, &config_path, json);
        }
        Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list,
            repin,
            json,
            nodes,
        }) => {
            if !json && nodes.is_empty() && !config_exists && repin.is_none() {
                println!("No config file was found or set.");
            }
            if let Some(name) = repin {
                run_repin(&name, &mut config, &config_path)?;
            } else {
                let action = NodeAction::from_flags(full_upgrade, reboot, dry_run, list)?;
                return run_node(action, nodes, &mut config, &config_path, json);
            }
        }
        Some(Commands::Profile { subcommand }) => {
            run_profile(subcommand, &mut config, &config_path)?
        }
        Some(Commands::Ca { subcommand }) => run_ca(subcommand)?,
        Some(Commands::Cert { subcommand }) => run_cert(subcommand)?,
        Some(Commands::Export { output, force }) => {
            export::run_export(&config, &ca::KeyringStore, &output, force, &config_path)?
        }
        Some(Commands::Import { .. }) => unreachable!("imports are handled before loading config"),
        Some(Commands::Version { json }) => return run_version(&mut config, &config_path, json),
        None => {
            // This case might be reached if only options are provided but no command.
            // However, clap handles --help and --version automatically.
            // If we want to show help by default when no command is provided:
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run_profile(
    subcommand: ProfileCommands,
    config: &mut Config,
    config_path: &Path,
) -> Result<(), Box<dyn Error>> {
    match subcommand {
        ProfileCommands::List => {
            for name in config.profiles.keys() {
                if name == &config.active_profile {
                    println!("* {}", name);
                } else {
                    println!("  {}", name);
                }
            }
        }
        ProfileCommands::Use { name } => {
            if config.profiles.contains_key(&name) {
                config.active_profile = name;
                save_config(config_path, config)?;
                println!("Switched to profile '{}'", config.active_profile);
            } else {
                return Err(format!("Profile '{}' not found", name).into());
            }
        }
        ProfileCommands::Create { name } => {
            if config.profiles.contains_key(&name) {
                return Err(format!("Profile '{}' already exists", name).into());
            }
            config
                .profiles
                .insert(name.clone(), ProfileConfig::default());
            save_config(config_path, config)?;
            println!("Created profile '{}'", name);
        }
        ProfileCommands::Delete { name } => {
            if name == default_profile_name() {
                return Err("Cannot delete the default profile".into());
            }
            if name == config.active_profile {
                return Err(
                    "Cannot delete the active profile. Switch to another profile first.".into(),
                );
            }
            if config.profiles.remove(&name).is_some() {
                save_config(config_path, config)?;
                println!("Deleted profile '{}'", name);
            } else {
                return Err(format!("Profile '{}' not found", name).into());
            }
        }
        ProfileCommands::Edit => {
            if !config_path.exists() {
                save_config(config_path, config)?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let mut parts = editor.split_whitespace();
            let program = parts.next().unwrap_or("vi");
            let extra_args: Vec<&str> = parts.collect();
            let status = std::process::Command::new(program)
                .args(extra_args)
                .arg(config_path)
                .status()?;
            if !status.success() {
                return Err(format!("Editor exited with status: {}", status).into());
            }
        }
    }
    Ok(())
}

fn run_ca(subcommand: CaCommands) -> Result<(), Box<dyn Error>> {
    match subcommand {
        CaCommands::Init { force } => {
            if !force && ca::ca_root_pem().is_ok() {
                return Err("cluster CA already exists; use --force to overwrite".into());
            }
            if force {
                let _ = ca::delete_credential("cluster-ca-cert");
                let _ = ca::delete_credential("cluster-ca-key");
            }
            let (cert, _key) = ca::ensure_ca()?;
            println!("Cluster CA created.");
            println!("  CN:   brewmble-cluster-ca");
            println!("  Days: {}", ca::CA_VALIDITY_DAYS);
            println!("  SHA256 fingerprint: {}", ca::fingerprint(&cert)?);
        }
        CaCommands::Show => {
            let cert = ca::ca_root_pem()?;
            println!("{}", cert);
            println!("Fingerprint: {}", ca::fingerprint(&cert)?);
        }
        CaCommands::Migrate { dry_run } => {
            let report = ca::migrate_ca(dry_run)?;
            if dry_run {
                println!("Dry run — no changes written.");
            }
            for entry in &report.entries {
                println!("{:<18} {}", entry.account, entry.status);
            }
            if !report.ca_ready() {
                return Err(
                    "CA certificate or key still missing after migration; run `brewmble ca init`"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

fn run_cert(subcommand: CertCommands) -> Result<(), Box<dyn Error>> {
    match subcommand {
        CertCommands::Issue {
            kind,
            cn,
            out_dir,
            role,
            force,
        } => match kind {
            CertKind::Daemon => {
                if role.is_some() {
                    return Err("--role applies only to client certificates".into());
                }
                let cn = cn.ok_or("--cn is required for daemon certificates")?;
                let out_dir = out_dir.ok_or("--out-dir is required for daemon certificates")?;
                std::fs::create_dir_all(&out_dir)?;

                let ca_path = out_dir.join("ca.pem");
                let cert_path = out_dir.join("daemon.pem");
                let key_path = out_dir.join("daemon-key.pem");

                if !force && (ca_path.exists() || cert_path.exists() || key_path.exists()) {
                    return Err("output files already exist; use --force to overwrite".into());
                }

                let (ca_cert, _ca_key) = ca::ensure_ca()?;
                let (cert, key) = ca::issue_leaf(&cn, ca::LeafKind::Daemon)?;

                std::fs::write(&ca_path, ca_cert)?;
                std::fs::write(&cert_path, &cert)?;
                write_private_key(&key_path, &key)?;

                println!("Daemon certificate written to {}", out_dir.display());
                println!("  CN:   {}", cn);
                println!("  SAN:  DNS:{}", cn);
                println!("  EKU:  ServerAuth");
                println!("  Days: {}", ca::LEAF_VALIDITY_DAYS);
                println!("  SHA256 fingerprint: {}", ca::fingerprint(&cert)?);
            }
            CertKind::Client => {
                let role: brewmble_rest::ClientRole = role.unwrap_or(CertRole::Operator).into();
                let cn =
                    cn.unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
                let cert = if let Some(out_dir) = out_dir {
                    std::fs::create_dir_all(&out_dir)?;
                    let ca_path = out_dir.join("ca.pem");
                    let cert_path = out_dir.join("client.pem");
                    let key_path = out_dir.join("client-key.pem");
                    if !force && (ca_path.exists() || cert_path.exists() || key_path.exists()) {
                        return Err("output files already exist; use --force to overwrite".into());
                    }
                    let (ca_cert, _ca_key) = ca::ensure_ca()?;
                    let (cert, key) = ca::issue_unstored_leaf(&cn, ca::LeafKind::Client(role))?;
                    std::fs::write(&ca_path, ca_cert)?;
                    std::fs::write(&cert_path, &cert)?;
                    write_private_key(&key_path, &key)?;
                    println!("Client certificate written to {}", out_dir.display());
                    cert
                } else {
                    if !force && ca::client_cert_pem().is_ok() {
                        return Err(
                            "client certificate already exists; use --force to re-issue".into()
                        );
                    }
                    let (cert, _key) = ca::issue_leaf(&cn, ca::LeafKind::Client(role))?;
                    println!("Client certificate issued and stored in the keyring.");
                    cert
                };
                let fingerprint = ca::fingerprint(&cert)?;
                println!("  CN:   {}", cn);
                println!("  SAN:  DNS:{}", cn);
                println!("  EKU:  ClientAuth");
                println!("  Role: {}", role);
                println!("  Days: {}", ca::LEAF_VALIDITY_DAYS);
                println!("  SHA256 fingerprint: {}", fingerprint);
                println!();
                println!("To allow this certificate, add its fingerprint to tls.allowed_clients");
                println!("in brewmbled.yaml on each node and reload the daemon:");
                println!("  allowed_clients:");
                println!("    - \"{}\"", fingerprint);
            }
        },
    }
    Ok(())
}

fn write_private_key(path: &Path, key: &str) -> Result<(), Box<dyn Error>> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.set_len(0)?;
    file.write_all(key.as_bytes())?;
    Ok(())
}

fn run_discover(
    timeout: Duration,
    update_config: bool,
    config_path: &Path,
) -> Result<(), Box<dyn Error>> {
    println!("Discovery will take {} seconds", timeout.as_secs());
    let mdns = ServiceDaemon::new().map_err(|err| format!("create resolver: {err}"))?;
    let service_name = format!("{}.{}", SERVICE_TYPE.trim_end_matches('.'), SERVICE_DOMAIN);
    let receiver = mdns
        .browse(&service_name)
        .map_err(|err| format!("browse: {err}"))?;

    let deadline = Instant::now() + timeout;
    let mut seen = HashSet::new();
    let mut header_printed = false;
    let mut discovered_nodes = Vec::new();

    let stdout = io::stdout();
    let mut writer = TabWriter::new(stdout).padding(2);

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline - now;
        match receiver.recv_timeout(remaining) {
            Ok(event) => match event {
                ServiceEvent::ServiceResolved(info) => {
                    let fullname = info.get_fullname().to_string();
                    if seen.insert(fullname) {
                        if !header_printed {
                            writeln!(writer, "ID\tHOST\tADDRESS\tPORT\tTLS\tINSTANCE")?;
                            header_printed = true;
                        }
                        writeln!(
                            writer,
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            entry_id(&info),
                            entry_host(&info),
                            entry_addresses(&info),
                            info.port,
                            entry_tls(&info),
                            entry_instance(&info)
                        )?;
                        writer.flush()?;

                        if let Some(addr) = info.addresses.iter().next() {
                            discovered_nodes
                                .push((format!("{}:{}", addr, info.port), entry_id(&info)));
                        }
                    }
                }
                ServiceEvent::SearchStopped(service_type) => {
                    eprintln!("Search stopped for {}", service_type);
                }
                _ => {}
            },
            Err(RecvTimeoutError::Timeout) => break,
            Err(RecvTimeoutError::Disconnected) => {
                return Err("browse: receiver disconnected".into());
            }
        }
    }

    let _ = mdns.shutdown();

    if !header_printed {
        println!("No brewmble daemons found.");
    }

    if update_config {
        let mut config = load_config(config_path)?;
        if merge_nodes(&mut config, discovered_nodes) {
            save_config(config_path, &config)?;
            println!("Configuration updated: {}", config_path.display());
        } else {
            println!("No new daemons found to add to configuration.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use mdns_sd::ServiceInfo;

    #[cfg(unix)]
    #[test]
    fn test_write_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("brewmble-key-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("client-key.pem");
        std::fs::write(&path, "old-longer-contents").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_key(&path, "key").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "key");

        let fresh = dir.join("fresh-key.pem");
        write_private_key(&fresh, "key").unwrap();
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn test_cli_parse_discover_default() {
        unsafe {
            std::env::remove_var("BREWMBLE_TIMEOUT");
        }
        let cli = Cli::parse_from(&["brewmble", "discover"]);
        if let Some(Commands::Discover {
            timeout,
            update_config,
        }) = cli.command
        {
            assert_eq!(timeout, 5);
            assert!(!update_config);
        } else {
            panic!("Wrong command");
        }
    }

    #[test]
    fn test_cli_parse_discover_timeout() {
        let cli = Cli::parse_from(&["brewmble", "discover", "-t", "10", "-u"]);
        if let Some(Commands::Discover {
            timeout,
            update_config,
        }) = cli.command
        {
            assert_eq!(timeout, 10);
            assert!(update_config);
        } else {
            panic!("Wrong command");
        }
    }

    #[test]
    fn test_cli_parse_node_short_upgrade() {
        let cli = Cli::parse_from(&["brewmble", "node", "-u"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run: _,
            list: _,
            repin: _,
            json: _,
            nodes: _,
        }) = cli.command
        {
            assert!(full_upgrade);
            assert!(!reboot);
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_node_dry_run() {
        let cli = Cli::parse_from(&["brewmble", "node", "--full-upgrade", "--dry-run"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list: _,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(full_upgrade);
            assert!(!reboot);
            assert!(dry_run);
            assert!(nodes.is_empty());
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_node_no_dry_run() {
        let cli = Cli::parse_from(&["brewmble", "node", "--full-upgrade", "node1"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list: _,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(full_upgrade);
            assert!(!reboot);
            assert!(!dry_run);
            assert_eq!(nodes, vec!["node1"]);
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_node_reboot() {
        let cli = Cli::parse_from(&["brewmble", "node", "-r", "node1"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list: _,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(!full_upgrade);
            assert!(reboot);
            assert!(!dry_run);
            assert_eq!(nodes, vec!["node1"]);
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_packages_alias() {
        let cli = Cli::parse_from(&["brewmble", "packages", "--full-upgrade", "node1"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list: _,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(full_upgrade);
            assert!(!reboot);
            assert!(!dry_run);
            assert_eq!(nodes, vec!["node1"]);
        } else {
            panic!("Expected Node command via packages alias");
        }
    }

    #[test]
    fn test_cli_parse_node_list() {
        let cli = Cli::parse_from(&["brewmble", "node", "--list"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(!full_upgrade);
            assert!(!reboot);
            assert!(!dry_run);
            assert!(list);
            assert!(nodes.is_empty());
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_node_list_short() {
        let cli = Cli::parse_from(&["brewmble", "node", "-l"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(!full_upgrade);
            assert!(!reboot);
            assert!(!dry_run);
            assert!(list);
            assert!(nodes.is_empty());
        } else {
            panic!("Expected Node command");
        }
    }

    #[test]
    fn test_cli_parse_packages_list() {
        let cli = Cli::parse_from(&["brewmble", "packages", "--list"]);
        if let Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list,
            repin: _,
            json: _,
            nodes,
        }) = cli.command
        {
            assert!(!full_upgrade);
            assert!(!reboot);
            assert!(!dry_run);
            assert!(list);
            assert!(nodes.is_empty());
        } else {
            panic!("Expected Node command via packages alias");
        }
    }

    #[test]
    fn test_cli_parse_node_list_conflicts_with_upgrade() {
        let result = Cli::try_parse_from(&["brewmble", "node", "--list", "--full-upgrade"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_node_list_conflicts_with_reboot() {
        let result = Cli::try_parse_from(&["brewmble", "node", "-l", "-r"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_node_list_conflicts_with_dry_run() {
        let result = Cli::try_parse_from(&["brewmble", "node", "--list", "--dry-run"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_node_reboot_conflicts_with_dry_run() {
        let result = Cli::try_parse_from(&["brewmble", "node", "--reboot", "--dry-run"]);
        assert!(result.is_err());
    }

    #[test]
    fn test_cli_parse_node_requires_action() {
        let result = Cli::try_parse_from(&["brewmble", "node", "node1"]);
        // Clap does not know the mutual requirement, so parsing succeeds and run_node errors.
        assert!(result.is_ok());
    }

    #[test]
    fn status_distinguishes_discovery_failure_from_an_empty_scan() {
        let config = Config::default();
        for json in [false, true] {
            let result = run_status(
                Some(|| Err("browse: simulated mDNS failure".to_string())),
                vec![],
                &config,
                Path::new(""),
                json,
            );
            assert_eq!(
                result.unwrap_err().to_string(),
                "mDNS discovery failed: browse: simulated mDNS failure"
            );
            let result = run_status(Some(|| Ok(vec![])), vec![], &config, Path::new(""), json);
            assert_eq!(result.unwrap(), ExitCode::SUCCESS);
        }
    }

    #[test]
    fn test_run_node_no_action_specified() {
        let err = NodeAction::from_flags(false, false, false, false)
            .err()
            .unwrap();
        assert_eq!(
            err.to_string(),
            "either --full-upgrade (-u), --reboot (-r), or --list (-l) must be specified"
        );
    }

    #[test]
    fn test_run_node_list_empty_profile() {
        let mut config = Config::default();
        let result = run_node(NodeAction::List, vec![], &mut config, Path::new(""), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_node_list_with_nodes() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            NodeConfig {
                name: Some("node-1".to_string()),
                address: "192.168.1.10:4712".to_string(),
                fingerprint: None,
            },
            NodeConfig {
                name: None,
                address: "192.168.1.50:4712".to_string(),
                fingerprint: None,
            },
        ];

        let result = run_node(NodeAction::List, vec![], &mut config, Path::new(""), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_node_list_specific_node() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("node-1".to_string()),
            address: "192.168.1.10:4712".to_string(),
            fingerprint: None,
        }];

        let result = run_node(
            NodeAction::List,
            vec!["node-1".to_string()],
            &mut config,
            Path::new(""),
            false,
        );
        assert!(result.is_ok());

        let err = run_node(
            NodeAction::List,
            vec!["unknown".to_string()],
            &mut config,
            Path::new(""),
            false,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Node 'unknown' not found in profile 'default'"
        );
    }

    #[test]
    fn test_resolve_profile_nodes_all() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            NodeConfig {
                name: Some("node-1".to_string()),
                address: "192.168.1.10:4712".to_string(),
                fingerprint: None,
            },
            NodeConfig {
                name: Some("node-2".to_string()),
                address: "192.168.1.11:4712".to_string(),
                fingerprint: None,
            },
        ];

        let resolved = resolve_profile_nodes(&[], &config).unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].name.as_deref(), Some("node-1"));
        assert_eq!(resolved[1].name.as_deref(), Some("node-2"));
    }

    #[test]
    fn test_resolve_profile_nodes_specific() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            NodeConfig {
                name: Some("node-1".to_string()),
                address: "192.168.1.10:4712".to_string(),
                fingerprint: None,
            },
            NodeConfig {
                name: Some("node-2".to_string()),
                address: "192.168.1.11:4712".to_string(),
                fingerprint: None,
            },
        ];

        let resolved = resolve_profile_nodes(&["node-2".to_string()], &config).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name.as_deref(), Some("node-2"));
        assert_eq!(resolved[0].address, "192.168.1.11:4712");
    }

    #[test]
    fn test_resolve_profile_nodes_not_found() {
        let config = Config::default();
        let err = resolve_profile_nodes(&["unknown-node".to_string()], &config).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Node 'unknown-node' not found in profile 'default'"
        );
    }

    #[test]
    fn test_resolve_profile_nodes_fallback_address() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: None,
            address: "192.168.1.50:4712".to_string(),
            fingerprint: None,
        }];

        let resolved = resolve_profile_nodes(&["192.168.1.50:4712".to_string()], &config).unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].address, "192.168.1.50:4712");
    }

    #[test]
    fn test_resolve_profile_nodes_missing_profile() {
        let mut config = Config::default();
        config.active_profile = "nonexistent".to_string();
        let err = resolve_profile_nodes(&[], &config).unwrap_err();
        assert_eq!(err.to_string(), "Profile 'nonexistent' not found");
    }

    #[test]
    fn test_cli_parse_ca_init() {
        let cli = Cli::parse_from(&["brewmble", "ca", "init"]);
        if let Some(Commands::Ca {
            subcommand: CaCommands::Init { force },
        }) = cli.command
        {
            assert!(!force);
        } else {
            panic!("Expected ca init command");
        }
    }

    #[test]
    fn test_cli_parse_ca_init_force() {
        let cli = Cli::parse_from(&["brewmble", "ca", "init", "--force"]);
        if let Some(Commands::Ca {
            subcommand: CaCommands::Init { force },
        }) = cli.command
        {
            assert!(force);
        } else {
            panic!("Expected ca init --force command");
        }
    }

    #[test]
    fn test_cli_parse_cert_issue_daemon() {
        let cli = Cli::parse_from(&[
            "brewmble",
            "cert",
            "issue",
            "--kind",
            "daemon",
            "--cn",
            "host1",
            "--out-dir",
            "/tmp/certs",
        ]);
        if let Some(Commands::Cert {
            subcommand:
                CertCommands::Issue {
                    kind: CertKind::Daemon,
                    cn,
                    out_dir,
                    role,
                    force,
                },
        }) = cli.command
        {
            assert_eq!(cn, Some("host1".to_string()));
            assert_eq!(out_dir, Some(PathBuf::from("/tmp/certs")));
            assert!(role.is_none());
            assert!(!force);
        } else {
            panic!("Expected cert issue daemon command");
        }
    }

    #[test]
    fn test_cli_parse_cert_issue_client_with_role() {
        let cli = Cli::parse_from(&[
            "brewmble",
            "cert",
            "issue",
            "--kind",
            "client",
            "--role",
            "read",
            "--cn",
            "monitor",
            "--out-dir",
            "/tmp/monitor",
        ]);
        if let Some(Commands::Cert {
            subcommand:
                CertCommands::Issue {
                    kind: CertKind::Client,
                    cn,
                    out_dir,
                    role,
                    ..
                },
        }) = cli.command
        {
            assert_eq!(cn.as_deref(), Some("monitor"));
            assert_eq!(out_dir, Some(PathBuf::from("/tmp/monitor")));
            assert_eq!(role, Some(CertRole::Read));
            assert_eq!(
                brewmble_rest::ClientRole::from(CertRole::Read),
                brewmble_rest::ClientRole::Read
            );
            assert_eq!(
                brewmble_rest::ClientRole::from(CertRole::Operator),
                brewmble_rest::ClientRole::Operator
            );
        } else {
            panic!("Expected cert issue client command");
        }
    }

    #[test]
    fn test_cert_issue_daemon_rejects_role() {
        let result = run_cert(CertCommands::Issue {
            kind: CertKind::Daemon,
            cn: Some("node1".to_string()),
            out_dir: Some(PathBuf::from("/nonexistent")),
            role: Some(CertRole::Read),
            force: false,
        });
        assert!(result.unwrap_err().to_string().contains("--role"));
    }

    #[test]
    fn test_cli_parse_cert_issue_client() {
        let cli = Cli::parse_from(&["brewmble", "cert", "issue", "--kind", "client"]);
        if let Some(Commands::Cert {
            subcommand:
                CertCommands::Issue {
                    kind: CertKind::Client,
                    cn,
                    out_dir,
                    role,
                    force,
                },
        }) = cli.command
        {
            assert!(cn.is_none());
            assert!(out_dir.is_none());
            assert!(role.is_none());
            assert!(!force);
        } else {
            panic!("Expected cert issue client command");
        }
    }

    #[test]
    fn test_cli_parse_export() {
        let cli = Cli::parse_from(["brewmble", "export", "--output", "backup.age", "--force"]);
        match cli.command {
            Some(Commands::Export { output, force }) => {
                assert_eq!(output, PathBuf::from("backup.age"));
                assert!(force);
            }
            _ => panic!("Expected export command"),
        }
    }

    #[test]
    fn test_cli_parse_import() {
        let cli = Cli::parse_from([
            "brewmble",
            "import",
            "--input",
            "backup.age",
            "--force",
            "--dry-run",
        ]);
        match cli.command {
            Some(Commands::Import {
                input,
                force,
                dry_run,
            }) => {
                assert_eq!(input, PathBuf::from("backup.age"));
                assert!(force);
                assert!(dry_run);
            }
            _ => panic!("Expected import command"),
        }
    }

    #[test]
    fn test_resolve_config_path() {
        let explicit = Some(PathBuf::from("custom.yaml"));
        let (path, exists) = resolve_config_path_inner(explicit, None, PathBuf::from("local.yaml"));
        assert_eq!(path, PathBuf::from("custom.yaml"));
        assert!(exists);

        let temp_dir = std::env::temp_dir().join(format!("brewmble_cfg_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let local = temp_dir.join(".brewmble.yaml");
        std::fs::write(&local, "profiles:\n").unwrap();

        let (path, exists) = resolve_config_path_inner(None, None, local.clone());
        assert_eq!(path, local);
        assert!(exists);

        let home = temp_dir.join("home");
        let _ = std::fs::create_dir_all(&home);
        let (path, exists) = resolve_config_path_inner(
            None,
            Some(home.clone()),
            PathBuf::from("does-not-exist.yaml"),
        );
        assert_eq!(path, home.join(".brewmble.yaml"));
        assert!(!exists);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_get_default_timeout() {
        unsafe {
            std::env::remove_var("BREWMBLE_TIMEOUT");
        }
        assert_eq!(get_default_timeout(), Duration::from_secs(60));

        unsafe {
            std::env::set_var("BREWMBLE_TIMEOUT", "15");
        }
        assert_eq!(get_default_timeout(), Duration::from_secs(15));

        unsafe {
            std::env::set_var("BREWMBLE_TIMEOUT", "1m");
        }
        assert_eq!(get_default_timeout(), Duration::from_secs(60));

        unsafe {
            std::env::remove_var("BREWMBLE_TIMEOUT");
        }
    }

    #[test]
    fn test_merge_nodes() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: None,
            address: "1.1.1.1:8080".to_string(),
            fingerprint: None,
        }];

        let discovered = vec![
            ("1.1.1.1:8080".to_string(), "node1".to_string()),
            ("2.2.2.2:8080".to_string(), "node2".to_string()),
        ];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes.len(), 2);

        // Existing node updated with name
        assert_eq!(nodes[0].address, "1.1.1.1:8080");
        assert_eq!(nodes[0].name, Some("node1".to_string()));

        // New node added with name
        assert_eq!(nodes[1].address, "2.2.2.2:8080");
        assert_eq!(nodes[1].name, Some("node2".to_string()));
    }

    #[test]
    fn test_save_status_pins_preserves_concurrent_profile_changes() {
        let dir = std::env::temp_dir().join(format!("brewmble_status_pins_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yaml");
        let node = |address: &str, fingerprint: Option<&str>| NodeConfig {
            name: None,
            address: address.to_string(),
            fingerprint: fingerprint.map(str::to_string),
        };
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            node("one:4712", None),
            node("two:4712", None),
            node("removed:4712", None),
        ];
        save_config(&path, &config).unwrap();
        let original = load_config(&path).unwrap();

        config.active_profile = "other".to_string();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            node("one:4712", None),
            node("two:4712", Some("newer-pin")),
            node("three:4712", None),
        ];
        config.profiles.insert(
            "other".to_string(),
            ProfileConfig {
                nodes: vec![node("one:4712", None)],
            },
        );
        save_config(&path, &config).unwrap();
        save_learned_status_pins(
            &path,
            "default",
            vec![
                (
                    &original.profiles["default"].nodes[0],
                    "learned".to_string(),
                ),
                (&original.profiles["default"].nodes[1], "stale".to_string()),
                (&original.profiles["default"].nodes[2], "stale".to_string()),
            ],
            false,
        )
        .unwrap();

        let latest = load_config(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(latest.active_profile, "other");
        assert!(latest.profiles["other"].nodes[0].fingerprint.is_none());
        let nodes = &latest.profiles["default"].nodes;
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].fingerprint.as_deref(), Some("learned"));
        assert_eq!(nodes[1].fingerprint.as_deref(), Some("newer-pin"));
        assert_eq!(nodes[2].address, "three:4712");
    }

    #[test]
    fn test_merge_nodes_updates_name() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("OldName".to_string()),
            address: "1.1.1.1:8080".to_string(),
            fingerprint: None,
        }];

        let discovered = vec![("1.1.1.1:8080".to_string(), "NewName".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes[0].name, Some("NewName".to_string()));
    }

    #[test]
    fn test_merge_nodes_updates_custom_name() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("Custom".to_string()),
            address: "1.1.1.1:8080".to_string(),
            fingerprint: None,
        }];

        let discovered = vec![("1.1.1.1:8080".to_string(), "node1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes[0].name, Some("node1".to_string()));
    }

    #[test]
    fn test_merge_nodes_cleans_id_prefix_from_config() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("id=raspi1".to_string()),
            address: "1.1.1.1:8080".to_string(),
            fingerprint: None,
        }];

        // Discovered node has the clean name
        let discovered = vec![("1.1.1.1:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes[0].name, Some("raspi1".to_string()));
    }

    #[test]
    fn test_merge_nodes_prevents_duplicate_by_name() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("raspi1".to_string()),
            address: "1.1.1.1:8080".to_string(),
            fingerprint: None,
        }];

        // raspi1 changed IP
        let discovered = vec![("1.1.1.2:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].address, "1.1.1.2:8080");
        assert_eq!(nodes[0].name, Some("raspi1".to_string()));
    }

    #[test]
    fn test_merge_nodes_clears_fingerprint_on_address_change() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("raspi1".to_string()),
            address: "1.1.1.1:8080".to_string(),
            fingerprint: Some("aa:bb".to_string()),
        }];

        let discovered = vec![("1.1.1.2:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes[0].address, "1.1.1.2:8080");
        assert!(nodes[0].fingerprint.is_none());
    }

    #[test]
    fn test_merge_nodes_preserves_saved_hostname_and_fingerprint() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("node1".to_string()),
            address: "node1.local:4712".to_string(),
            fingerprint: Some("aa:bb".to_string()),
        }];

        let updated = merge_nodes(
            &mut config,
            vec![("192.0.2.10:4712".to_string(), "node1".to_string())],
        );
        assert!(!updated);
        let node = &config.profiles["default"].nodes[0];
        assert_eq!(node.address, "node1.local:4712");
        assert_eq!(node.fingerprint.as_deref(), Some("aa:bb"));
    }

    #[test]
    fn test_clean_node_id() {
        assert_eq!(clean_node_id("id=raspi1"), "raspi1");
        assert_eq!(clean_node_id("raspi1"), "raspi1");
        assert_eq!(clean_node_id(""), "");
    }

    #[test]
    fn test_resolve_url() {
        assert_eq!(resolve_url("1.2.3.4:8080"), "https://1.2.3.4:8080");
        assert_eq!(resolve_url("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(resolve_url("https://example.com"), "https://example.com");
        assert_eq!(resolve_url("example.com:80"), "https://example.com:80");
        assert_eq!(resolve_url("::1:8080"), "https://[::1]:8080");
        assert_eq!(resolve_url("[::1]:8080"), "https://[::1]:8080");
        assert_eq!(resolve_url("::1"), "https://[::1]");
        assert_eq!(resolve_url("localhost"), "https://localhost");
        assert_eq!(resolve_url("1.2.3.4:8080/"), "https://1.2.3.4:8080");
    }

    #[test]
    fn test_entry_helpers() {
        let properties = [("id", "node1"), ("tls", "mtls")];
        let info = ServiceInfo::new(
            "_brewmble._tcp.local.",
            "brewmbled-node1",
            "node1.local.",
            "1.2.3.4",
            8080,
            &properties[..],
        )
        .unwrap();

        let resolved = info.as_resolved_service();

        assert_eq!(entry_id(&resolved), "node1");
        assert_eq!(entry_host(&resolved), "node1.local");
        assert_eq!(entry_addresses(&resolved), "1.2.3.4");
        assert_eq!(entry_tls(&resolved), "mtls");
        assert_eq!(entry_instance(&resolved), "brewmbled-node1");
    }

    #[test]
    fn test_config_migration() {
        let yaml = r#"
nodes:
  - name: legacy-node
    address: "1.2.3.4:8080"
    api_key: "secret-token"
"#;
        let mut config: Config = serde_yaml_ng::from_str(yaml).unwrap();

        // Before migration check
        assert_eq!(config.nodes.len(), 1);
        assert!(config.profiles.is_empty());

        // Perform migration (as in load_config)
        if !config.nodes.is_empty() {
            let profile = config
                .profiles
                .entry(default_profile_name())
                .or_insert_with(ProfileConfig::default);
            profile.nodes.extend(config.nodes.drain(..));
        }

        assert_eq!(config.nodes.len(), 0);
        assert_eq!(config.profiles.len(), 1);
        let profile = config.profiles.get("default").unwrap();
        assert_eq!(profile.nodes.len(), 1);
        assert_eq!(profile.nodes[0].name, Some("legacy-node".to_string()));
    }
}

fn clean_node_id(id: &str) -> &str {
    id.strip_prefix("id=").unwrap_or(id)
}

fn entry_id(entry: &ResolvedService) -> String {
    entry
        .txt_properties
        .iter()
        .find(|p| p.key() == "id")
        .map(|p| sanitize_display(clean_node_id(p.val_str())))
        .unwrap_or_default()
}

fn entry_host(entry: &ResolvedService) -> String {
    sanitize_display(entry.host.trim_end_matches('.'))
}

fn entry_addresses(entry: &ResolvedService) -> String {
    entry
        .addresses
        .iter()
        .map(|addr| addr.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn entry_instance(entry: &ResolvedService) -> String {
    let fullname = &entry.fullname;
    let suffix = format!(".{}.{}", SERVICE_TYPE.trim_end_matches('.'), SERVICE_DOMAIN);
    sanitize_display(fullname.strip_suffix(&suffix).unwrap_or(fullname))
}

fn entry_tls(entry: &ResolvedService) -> String {
    entry
        .txt_properties
        .iter()
        .find(|p| p.key() == TXT_TLS)
        .map(|p| sanitize_display(p.val_str()))
        .unwrap_or_default()
}

fn run_version(
    config: &mut Config,
    config_path: &Path,
    json: bool,
) -> Result<ExitCode, Box<dyn Error>> {
    #[derive(Serialize)]
    struct VersionResult {
        daemon_version: Option<String>,
    }

    let nodes = resolve_profile_nodes(&[], config)?
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let mut reports = Vec::new();
    if !nodes.is_empty() {
        let clients = pin::PinnedClients::load()?;
        let mut pin_updated = false;
        for node in &nodes {
            let (client, outcome) = match clients.connect(node) {
                Ok(connection) => connection,
                Err(error) => {
                    reports.push(pin_failure(node, error));
                    continue;
                }
            };
            if let PinOutcome::Learned(fp) = outcome {
                print_learned_pin(node, &fp, json);
                update_node_fingerprint(config, &node.address, &fp);
                pin_updated = true;
            }
            reports.push(query_status(&client, node).map(|status| VersionResult {
                daemon_version: status.daemon_version,
            }));
        }

        if pin_updated {
            save_config(config_path, config)?;
        }
    }

    if json {
        fleet::write_json(io::stdout().lock(), &reports)?;
    } else {
        let mut tw = TabWriter::new(io::stdout());
        writeln!(tw, "COMPONENT\tVERSION")?;
        writeln!(tw, "CLI (local)\t{}", env!("CARGO_PKG_VERSION"))?;
        for report in &reports {
            let version = match &report.outcome {
                Ok(result) => result.daemon_version.as_deref().unwrap_or("Unknown"),
                Err(error) => error,
            };
            writeln!(tw, "{}\t{}", report.name, version)?;
        }
        tw.flush()?;
    }
    Ok(fleet::exit_code(&reports))
}

fn run_status(
    discover: Option<impl FnOnce() -> Result<Vec<(String, String)>, String> + Send + 'static>,
    targets: Vec<String>,
    config: &Config,
    config_path: &Path,
    json: bool,
) -> Result<ExitCode, Box<dyn Error>> {
    let active_profile_name = config.active_profile.clone();
    let profile_nodes = config
        .profiles
        .get(&active_profile_name)
        .map(|p| p.nodes.clone())
        .unwrap_or_default();
    if !json {
        println!("Querying status...");
    }

    let multi = MultiProgress::new();
    let native = (!json).then(NativeProgress::new);
    let native_supported = native.as_ref().is_some_and(NativeProgress::is_supported);
    let spinner_style = ProgressStyle::default_spinner()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈")
        .template("{spinner:.green} {msg}")?;

    let (target_tx, target_rx) = flume::unbounded::<(String, Option<String>)>();

    let saved_targets: HashMap<_, _> = profile_nodes
        .iter()
        .rev()
        .filter_map(|node| {
            reqwest::Url::parse(&resolve_url(&node.address))
                .ok()
                .map(|url| (url, node))
        })
        .collect();

    // 1. Collect explicit targets and profile nodes, deduplicated by address
    let mut initial_targets: HashSet<String> = HashSet::new();
    for t in targets {
        let addr = profile_nodes
            .iter()
            .find(|n| n.address == t || n.name.as_ref() == Some(&t))
            .map(|n| n.address.clone())
            .unwrap_or(t);
        initial_targets.insert(addr);
    }

    if initial_targets.is_empty() && discover.is_none() {
        for node in &profile_nodes {
            initial_targets.insert(node.address.clone());
        }
    }

    for addr in initial_targets {
        let name = profile_nodes
            .iter()
            .find(|n| n.address == addr)
            .and_then(|n| n.name.clone());
        target_tx.send((addr, name))?;
    }

    // 2. Start discovery in background if requested
    let discover_was_requested = discover.is_some();
    let discovery_worker = discover.map(|discover| {
        if let Some(native) = &native {
            native.set_indeterminate();
        }
        let pb = if native_supported || json {
            let pb = ProgressBar::new_spinner();
            pb.set_draw_target(ProgressDrawTarget::hidden());
            pb
        } else {
            multi.add(ProgressBar::new_spinner())
        };
        pb.set_style(spinner_style.clone());
        pb.set_message("Discovering nodes...");
        let tx = target_tx.clone();
        std::thread::spawn(move || {
            let result = discover();
            pb.finish_and_clear();
            for (addr, name) in result.map_err(|error| format!("mDNS discovery failed: {error}"))? {
                let _ = tx.send((addr, Some(name)));
            }
            Ok::<(), String>(())
        })
    });

    drop(target_tx); // Close tx so rx finishes when discovery is done

    let seen_targets = Mutex::new(HashSet::new());
    let node_reports = Mutex::new(Vec::new());
    let learned_pins = Mutex::new(Vec::new());

    let pb = if native_supported || json {
        let pb = ProgressBar::new(0);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb
    } else {
        multi.add(ProgressBar::new(0))
    };
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )?
            .progress_chars("#>-"),
    );
    pb.set_message("Querying status...");

    let mut targets = target_rx.iter().peekable();
    let clients = targets
        .peek()
        .map(|_| pin::PinnedClients::load())
        .transpose()?;
    rayon::scope(|s| {
        let Some(clients) = &clients else {
            return;
        };
        for (target_addr, target_name) in targets {
            let seen_targets = &seen_targets;
            let node_reports = &node_reports;
            let learned_pins = &learned_pins;
            let saved_node = reqwest::Url::parse(&resolve_url(&target_addr))
                .ok()
                .and_then(|url| saved_targets.get(&url).copied());
            let pb = pb.clone();
            let multi = multi.clone();
            let native = &native;

            pb.inc_length(1);
            if let Some(native) = native {
                native.set_progress(pb.position(), pb.length());
            }

            s.spawn(move |_| {
                let address = saved_node.map_or(target_addr.as_str(), |node| &node.address);
                if !seen_targets.lock().unwrap().insert(address.to_owned()) {
                    pb.inc(1);
                    if let Some(native) = native {
                        native.set_progress(pb.position(), pb.length());
                    }
                    return;
                }

                let discovered_node = NodeConfig {
                    address: target_addr.clone(),
                    name: target_name.clone(),
                    fingerprint: None,
                };
                let node = saved_node.unwrap_or(&discovered_node);
                let report = match clients.connect(node) {
                    Ok((client, outcome)) => {
                        if let (Some(saved), PinOutcome::Learned(fp)) = (saved_node, outcome) {
                            learned_pins.lock().unwrap().push((saved, fp));
                        }
                        query_status(&client, node)
                    }
                    Err(error) => pin_failure(node, error),
                };

                if !json {
                    let _ = multi.println(format_status_report(&report));
                }
                node_reports.lock().unwrap().push(report);
                pb.inc(1);
                if let Some(native) = native {
                    native.set_progress(pb.position(), pb.length());
                }
            });
        }
    });

    pb.finish_and_clear();
    if let Some(worker) = discovery_worker {
        worker
            .join()
            .map_err(|_| "mDNS discovery worker panicked")??;
    }

    save_learned_status_pins(
        config_path,
        &active_profile_name,
        learned_pins.into_inner().unwrap(),
        json,
    )?;

    let mut reports = node_reports.into_inner().unwrap();
    let exit_code = fleet::exit_code(&reports);
    if json {
        reports.sort_by(|a, b| a.address.cmp(&b.address));
        fleet::write_json(io::stdout().lock(), &reports)?;
    } else if !reports.is_empty() {
        print_status_summary(&reports);
    }

    if let Some(native) = native.filter(NativeProgress::is_supported)
        && (!reports.is_empty() || discover_was_requested)
    {
        if exit_code == ExitCode::FAILURE {
            native.finish_error();
        } else {
            native.finish_success();
        }
        std::thread::sleep(Duration::from_millis(500));
        native.clear();
    }

    if let Some(clients) = clients {
        clients.finish()?;
    }
    Ok(exit_code)
}

fn pin_failure<T>(node: &NodeConfig, error: Box<dyn Error>) -> NodeReport<T> {
    NodeReport::failed(node, format!("Pin verification failed: {error}"))
}

fn query_status(
    client: &reqwest::blocking::Client,
    node: &NodeConfig,
) -> NodeReport<StatusResponse> {
    let url = resolve_url(&node.address);
    NodeReport::request(node, client.get(format!("{url}{PATH_STATUS}")))
}

fn format_status_report(report: &NodeReport<StatusResponse>) -> String {
    let mut output = format!("Node: {}\nAddress: {}\n", report.name, report.address);
    if let Ok(status) = &report.outcome
        && !status.message.is_empty()
    {
        let _ = writeln!(output, "{}", status.message);
    }
    let _ = writeln!(output, "Details:");
    if let Some(http_status) = report.http_status {
        let _ = writeln!(output, "    Status: {http_status}");
    }
    match &report.outcome {
        Ok(status) => {
            if let Some(version) = &status.daemon_version {
                let _ = writeln!(output, "    Daemon version: {version}");
            }
            let _ = writeln!(output, "    Upgrading: {}", status.is_upgrading);
            let _ = writeln!(output, "    Allow reboot: {}", status.allow_reboot);
            let _ = writeln!(output, "    Auto clean: {}", status.auto_clean);
            let _ = writeln!(output, "    Auto remove: {}", status.auto_remove);
            if status.updates.is_empty() {
                let _ = writeln!(output, "    No updates available.");
            } else {
                let _ = writeln!(output, "    Updates:");
                for update in &status.updates {
                    let _ = writeln!(output, "      - {update}");
                }
            }
        }
        Err(error) => {
            let _ = writeln!(output, "    {error}");
        }
    }
    let _ = writeln!(output);
    output
}

fn print_status_summary(reports: &[NodeReport<StatusResponse>]) {
    println!("Summary:");
    let outdated_nodes: Vec<_> = reports
        .iter()
        .filter(|report| {
            report
                .outcome
                .as_ref()
                .is_ok_and(|status| !status.updates.is_empty())
        })
        .map(|report| report.name.as_str())
        .collect();
    let failed_nodes: Vec<_> = reports
        .iter()
        .filter(|report| report.outcome.is_err())
        .map(|report| report.name.as_str())
        .collect();

    if !outdated_nodes.is_empty() {
        println!(
            "- {} node(s) are outdated: {}",
            outdated_nodes.len(),
            outdated_nodes.join(", ")
        );
    } else if failed_nodes.is_empty() {
        println!("- All nodes are up to date.");
    }

    if !failed_nodes.is_empty() {
        println!(
            "- {} node(s) failed: {}",
            failed_nodes.len(),
            failed_nodes.join(", ")
        );
    }
}

fn discover_targets_with_timeout(timeout: Duration) -> Result<Vec<(String, String)>, String> {
    let mut targets = Vec::new();
    let mdns = ServiceDaemon::new().map_err(|err| format!("create resolver: {err}"))?;
    let service_name = format!("{}.{}", SERVICE_TYPE.trim_end_matches('.'), SERVICE_DOMAIN);
    let receiver = mdns
        .browse(&service_name)
        .map_err(|err| format!("browse: {err}"))?;

    let deadline = Instant::now() + timeout;
    let mut seen = HashSet::new();

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }

        let remaining = deadline - now;
        match receiver.recv_timeout(remaining) {
            Ok(event) => {
                if let ServiceEvent::ServiceResolved(info) = event {
                    let name = entry_instance(&info);
                    for addr in &info.addresses {
                        let target = format!("{}:{}", addr, info.port);
                        if seen.insert(target.clone()) {
                            targets.push((target, name.clone()));
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(err) => return Err(format!("mDNS error: {err}")),
        }
    }
    let _ = mdns.shutdown();
    Ok(targets)
}

pub(crate) fn resolve_url(target: &str) -> String {
    use std::net::{IpAddr, SocketAddr};

    if target.starts_with("http://") || target.starts_with("https://") {
        target.trim_end_matches('/').to_string()
    } else if let Ok(addr) = target.parse::<SocketAddr>() {
        match addr {
            SocketAddr::V6(v6) => format!("https://[{}]:{}", v6.ip(), v6.port()),
            SocketAddr::V4(v4) => format!("https://{}:{}", v4.ip(), v4.port()),
        }
    } else if let Ok(IpAddr::V6(v6)) = target.parse::<IpAddr>() {
        let is_ipv6_with_port = target.matches(':').count() > 2
            && target
                .rsplit_once(':')
                .map_or(false, |(_, p)| p.chars().all(|c| c.is_ascii_digit()));

        if is_ipv6_with_port {
            let parts: Vec<&str> = target.split(':').collect();
            let host = parts[..parts.len() - 1].join(":");
            let port = parts.last().unwrap();
            format!("https://[{}]:{}", host, port)
        } else {
            format!("https://[{}]", v6)
        }
    } else if target.contains(':')
        && target
            .split(':')
            .last()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        let parts: Vec<&str> = target.split(':').collect();
        let host = parts[..parts.len() - 1].join(":");
        let port = parts.last().unwrap();

        if host.contains(':') && !host.starts_with('[') {
            format!("https://[{}]:{}", host, port)
        } else {
            format!("https://{}:{}", host, port)
        }
    } else {
        format!("https://{}", target.trim_end_matches('/'))
    }
}

fn resolve_profile_nodes<'a>(
    nodes: &[String],
    config: &'a Config,
) -> Result<Vec<&'a NodeConfig>, Box<dyn Error>> {
    let profile = config
        .profiles
        .get(&config.active_profile)
        .ok_or_else(|| format!("Profile '{}' not found", config.active_profile))?;

    if nodes.is_empty() {
        Ok(profile.nodes.iter().collect())
    } else {
        let mut matched = Vec::new();
        for node_name in nodes {
            if let Some(node) = profile.nodes.iter().find(|n| {
                n.name.as_deref() == Some(node_name.as_str())
                    || (n.name.is_none() && &n.address == node_name)
            }) {
                matched.push(node);
            } else {
                return Err(format!(
                    "Node '{}' not found in profile '{}'",
                    node_name, config.active_profile
                )
                .into());
            }
        }
        Ok(matched)
    }
}

fn run_repin(name: &str, config: &mut Config, config_path: &Path) -> Result<(), Box<dyn Error>> {
    let active_profile = config.active_profile.clone();
    let address = config
        .profiles
        .get(&active_profile)
        .and_then(|p| {
            p.nodes
                .iter()
                .find(|n| n.name.as_deref() == Some(name) || n.address == name)
                .map(|n| n.address.clone())
        })
        .ok_or_else(|| format!("Node '{}' not found in profile '{}'", name, active_profile))?;

    update_node_fingerprint(config, &address, "");
    if let Some(profile) = config.profiles.get_mut(&active_profile) {
        if let Some(node) = profile.nodes.iter_mut().find(|n| n.address == address) {
            node.fingerprint = None;
        }
    }

    let node = config
        .profiles
        .get(&active_profile)
        .unwrap()
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .unwrap();

    match check_pin(&node, &node.address) {
        Ok(PinOutcome::Learned(fp)) => {
            println!("Re-pinned {} {}", pin::node_name(&node), fp);
            update_node_fingerprint(config, &node.address, &fp);
            save_config(config_path, config)?;
            Ok(())
        }
        Ok(PinOutcome::Matched) => {
            println!(
                "Re-pinned {} (fingerprint unchanged)",
                pin::node_name(&node)
            );
            Ok(())
        }
        Err(e) => Err(e),
    }
}

enum NodeAction {
    Upgrade { dry_run: bool },
    Reboot,
    List,
}

impl NodeAction {
    fn from_flags(
        full_upgrade: bool,
        reboot: bool,
        dry_run: bool,
        list: bool,
    ) -> Result<Self, Box<dyn Error>> {
        if dry_run && !full_upgrade {
            return Err("--dry-run can only be used with --full-upgrade".into());
        }
        if list {
            Ok(Self::List)
        } else if reboot {
            Ok(Self::Reboot)
        } else if full_upgrade {
            Ok(Self::Upgrade { dry_run })
        } else {
            Err(
                "either --full-upgrade (-u), --reboot (-r), or --list (-l) must be specified"
                    .into(),
            )
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum NodeActionResult {
    Upgrade(UpgradeResponse),
    Reboot(RebootResponse),
}

fn run_node(
    action: NodeAction,
    nodes: Vec<String>,
    config: &mut Config,
    config_path: &Path,
    json: bool,
) -> Result<ExitCode, Box<dyn Error>> {
    let target_nodes: Vec<NodeConfig> = resolve_profile_nodes(&nodes, config)?
        .into_iter()
        .cloned()
        .collect();

    if target_nodes.is_empty() {
        if json {
            fleet::write_json::<NodeActionResult>(io::stdout().lock(), &[])?;
        } else {
            println!("No nodes found.");
        }
        return Ok(ExitCode::SUCCESS);
    }

    if matches!(action, NodeAction::List) {
        for node in &target_nodes {
            println!("{}", pin::node_name(node));
        }
        return Ok(ExitCode::SUCCESS);
    }

    let clients = pin::PinnedClients::load()?;
    let mut reports = Vec::new();
    for node in &target_nodes {
        let (client, outcome) = match clients.connect(node) {
            Ok(connection) => connection,
            Err(error) => {
                reports.push(pin_failure(node, error));
                continue;
            }
        };
        if let PinOutcome::Learned(fp) = outcome {
            print_learned_pin(node, &fp, json);
            update_node_fingerprint(config, &node.address, &fp);
            save_config(config_path, config)?;
        }

        let url = resolve_url(&node.address);
        let report = match action {
            NodeAction::Reboot => {
                let request = client
                    .post(format!("{url}{PATH_REBOOT}"))
                    .json(&brewmble_rest::RebootRequest::default());
                NodeReport::request(node, request).map(NodeActionResult::Reboot)
            }
            NodeAction::Upgrade { dry_run } => {
                let request = client
                    .post(format!("{url}{PATH_UPGRADE}"))
                    .json(&brewmble_rest::UpgradeRequest { dry_run });
                NodeReport::request(node, request).map(NodeActionResult::Upgrade)
            }
            NodeAction::List => unreachable!(),
        };
        reports.push(report);
    }

    if json {
        fleet::write_json(io::stdout().lock(), &reports)?;
    } else {
        print_node_reports(&reports)?;
    }
    clients.finish()?;
    Ok(fleet::exit_code(&reports))
}

fn print_node_reports(reports: &[NodeReport<NodeActionResult>]) -> io::Result<()> {
    let mut tw = TabWriter::new(io::stdout());
    writeln!(tw, "NODE\tSTATUS")?;
    for report in reports {
        let name = &report.name;
        if let Some(status) = report.http_status {
            writeln!(tw, "{name}\t{status}")?;
        }
        let (message, updates) = match &report.outcome {
            Ok(NodeActionResult::Reboot(response)) => (&response.message, None),
            Ok(NodeActionResult::Upgrade(response)) => {
                (&response.message, response.updates.as_ref())
            }
            Err(error) => {
                if report.http_status.is_some() {
                    writeln!(tw, "\t{}", error.replace('\n', "\n\t"))?;
                } else {
                    writeln!(tw, "{name}\t{error}")?;
                }
                continue;
            }
        };
        if !message.is_empty() {
            writeln!(tw, "\t{}", message.replace('\n', "\n\t"))?;
        }
        if let Some(updates) = updates.filter(|updates| !updates.is_empty()) {
            writeln!(tw, "\tUpdates ({}): {}", updates.len(), updates.join(", "))?;
        }
    }
    tw.flush()
}
