use brewmble_rest::{
    RebootResponse, StatusResponse, UpgradeResponse, PATH_REBOOT, PATH_STATUS, PATH_UPGRADE,
    SERVICE_DOMAIN, SERVICE_TYPE, TXT_TLS,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tabwriter::TabWriter;

mod ca;
mod progress;
use progress::NativeProgress;

#[derive(Serialize, Deserialize, Debug)]
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
struct NodeConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    address: String,
}

fn resolve_config_path(explicit_path: Option<PathBuf>) -> (PathBuf, bool) {
    if let Some(path) = explicit_path {
        return (path, true);
    }

    if let Some(mut home) = home::home_dir() {
        home.push(".brewmble.yaml");
        if home.exists() {
            return (home, true);
        }
    }

    let local_path = PathBuf::from(".brewmble.yaml");
    if local_path.exists() {
        (local_path, true)
    } else {
        // Default to home dir if it exists
        if let Some(mut home) = home::home_dir() {
            home.push(".brewmble.yaml");
            (home, false)
        } else {
            (local_path, false)
        }
    }
}

fn load_config(path: &Path) -> Result<Config, Box<dyn Error>> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let content = fs::read_to_string(path)?;
    let mut config: Config = serde_yaml::from_str(&content)?;

    // Migration: Move old nodes to default profile
    if !config.nodes.is_empty() {
        let profile = config
            .profiles
            .entry(default_profile_name())
            .or_insert_with(ProfileConfig::default);
        profile.nodes.extend(config.nodes.drain(..));
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
    let content = serde_yaml::to_string(config)?;
    fs::write(path, content)?;
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
            if node.address != addr {
                node.address = addr;
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
            });
            updated = true;
        }
    }
    updated
}

fn get_default_timeout() -> Duration {
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
        #[arg(short = 'l', long, conflicts_with_all = ["full_upgrade", "reboot", "dry_run"])]
        list: bool,

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
    /// Show version information for CLI and daemons
    Version,
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
        /// Output directory for daemon certificate files
        #[arg(short, long)]
        out_dir: Option<PathBuf>,
        /// Overwrite existing client certificate or output files
        #[arg(long)]
        force: bool,
    },
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

fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    if cli.version {
        println!("brewmble {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (config_path, config_exists) = resolve_config_path(cli.config);
    let mut config = match load_config(&config_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: failed to load config: {err}");
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Some(Commands::Discover {
            timeout,
            update_config,
        }) => run_discover(Duration::from_secs(timeout), update_config, &config_path),
        Some(Commands::Status { all, targets }) => {
            if targets.is_empty() && !all && !config_exists {
                println!("No config file was found or set.");
            }
            run_status(all, targets, &config)
        }
        Some(Commands::Node {
            full_upgrade,
            reboot,
            dry_run,
            list,
            nodes,
        }) => {
            if nodes.is_empty() && !config_exists {
                println!("No config file was found or set.");
            }
            run_node(full_upgrade, reboot, dry_run, list, nodes, &config)
        }
        Some(Commands::Profile { subcommand }) => {
            run_profile(subcommand, &mut config, &config_path)
        }
        Some(Commands::Ca { subcommand }) => run_ca(subcommand),
        Some(Commands::Cert { subcommand }) => run_cert(subcommand),
        Some(Commands::Version) => run_version(&config),
        None => {
            // This case might be reached if only options are provided but no command.
            // However, clap handles --help and --version automatically.
            // If we want to show help by default when no command is provided:
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
    };

    if let Err(err) = result {
        return Err(err);
    }
    Ok(())
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
            let status = std::process::Command::new(editor)
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
            if !force && ca::ensure_ca().is_ok_and(|(c, k)| !c.is_empty() && !k.is_empty()) {
                return Err("cluster CA already exists; use --force to overwrite".into());
            }
            if force {
                let _ = keyring::Entry::new("brewmble-cli", "cluster-ca-cert")
                    .and_then(|e| e.delete_password());
                let _ = keyring::Entry::new("brewmble-cli", "cluster-ca-key")
                    .and_then(|e| e.delete_password());
            }
            let (cert, _key) = ca::ensure_ca()?;
            println!("Cluster CA created.");
            println!("Fingerprint: {}", ca::fingerprint(&cert)?);
        }
        CaCommands::Show => {
            let entry = keyring::Entry::new("brewmble-cli", "cluster-ca-cert")?;
            let cert = entry.get_password()?;
            println!("{}", cert);
            println!("Fingerprint: {}", ca::fingerprint(&cert)?);
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
            force,
        } => match kind {
            CertKind::Daemon => {
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
                std::fs::write(&cert_path, cert)?;
                std::fs::write(&key_path, key)?;

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = std::fs::metadata(&key_path)?.permissions();
                    perms.set_mode(0o600);
                    std::fs::set_permissions(&key_path, perms)?;
                }

                println!("Daemon certificate written to {}", out_dir.display());
            }
            CertKind::Client => {
                if !force
                    && keyring::Entry::new("brewmble-cli", "client-cert")
                        .and_then(|e| e.get_password())
                        .is_ok()
                {
                    return Err("client certificate already exists; use --force to re-issue".into());
                }
                let hostname = gethostname::gethostname().to_string_lossy().into_owned();
                ca::issue_leaf(&hostname, ca::LeafKind::Client)?;
                println!("Client certificate issued and stored in the keyring.");
            }
        },
    }
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
    fn test_run_node_no_action_specified() {
        let config = Config::default();
        let err = run_node(false, false, false, false, vec![], &config).unwrap_err();
        assert_eq!(
            err.to_string(),
            "either --full-upgrade (-u), --reboot (-r), or --list (-l) must be specified"
        );
    }

    #[test]
    fn test_run_node_list_empty_profile() {
        let config = Config::default();
        let result = run_node(false, false, false, true, vec![], &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_node_list_with_nodes() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            NodeConfig {
                name: Some("node-1".to_string()),
                address: "192.168.1.10:4712".to_string(),
            },
            NodeConfig {
                name: None,
                address: "192.168.1.50:4712".to_string(),
            },
        ];

        let result = run_node(false, false, false, true, vec![], &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_node_list_specific_node() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![
            NodeConfig {
                name: Some("node-1".to_string()),
                address: "192.168.1.10:4712".to_string(),
            },
        ];

        let result = run_node(
            false,
            false,
            false,
            true,
            vec!["node-1".to_string()],
            &config,
        );
        assert!(result.is_ok());

        let err = run_node(
            false,
            false,
            false,
            true,
            vec!["unknown".to_string()],
            &config,
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
            },
            NodeConfig {
                name: Some("node-2".to_string()),
                address: "192.168.1.11:4712".to_string(),
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
            },
            NodeConfig {
                name: Some("node-2".to_string()),
                address: "192.168.1.11:4712".to_string(),
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
        }];

        let resolved =
            resolve_profile_nodes(&["192.168.1.50:4712".to_string()], &config).unwrap();
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
                    force,
                },
        }) = cli.command
        {
            assert_eq!(cn, Some("host1".to_string()));
            assert_eq!(out_dir, Some(PathBuf::from("/tmp/certs")));
            assert!(!force);
        } else {
            panic!("Expected cert issue daemon command");
        }
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
                    force,
                },
        }) = cli.command
        {
            assert!(cn.is_none());
            assert!(out_dir.is_none());
            assert!(!force);
        } else {
            panic!("Expected cert issue client command");
        }
    }

    #[test]
    fn test_resolve_config_path() {
        let explicit = Some(PathBuf::from("custom.yaml"));
        let (path, exists) = resolve_config_path(explicit);
        assert_eq!(path, PathBuf::from("custom.yaml"));
        assert!(exists);

        let (path, _) = resolve_config_path(None);
        assert!(path.to_string_lossy().contains(".brewmble.yaml"));
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
    fn test_merge_nodes_updates_name() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("OldName".to_string()),
            address: "1.1.1.1:8080".to_string(),
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
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();

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
        .map(|p| clean_node_id(p.val_str()).to_string())
        .unwrap_or_default()
}

fn entry_host(entry: &ResolvedService) -> String {
    entry.host.trim_end_matches('.').to_string()
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
    fullname
        .strip_suffix(&suffix)
        .unwrap_or(fullname)
        .to_string()
}

fn entry_tls(entry: &ResolvedService) -> String {
    entry
        .txt_properties
        .iter()
        .find(|p| p.key() == TXT_TLS)
        .map(|p| p.val_str().to_string())
        .unwrap_or_default()
}

fn run_version(config: &Config) -> Result<(), Box<dyn Error>> {
    let mut tw = TabWriter::new(std::io::stdout());
    writeln!(tw, "COMPONENT\tVERSION")?;
    writeln!(tw, "CLI (local)\t{}", env!("CARGO_PKG_VERSION"))?;

    let active_profile = config.profiles.get(&config.active_profile);
    if let Some(profile) = active_profile {
        let client = ca::build_client()?;

        for node in &profile.nodes {
            let url = resolve_url(&node.address);
            let status_url = format!("{}{}", url, PATH_STATUS);
            let request = client.get(&status_url);

            let version = match request.send() {
                Ok(resp) if resp.status().is_success() => match resp.json::<StatusResponse>() {
                    Ok(sr) => sr.daemon_version.unwrap_or_else(|| "Unknown".to_string()),
                    Err(_) => "Unknown (Parse Error)".to_string(),
                },
                Ok(resp) => format!("Error ({})", resp.status()),
                Err(_) => "Unreachable".to_string(),
            };

            let name = node.name.as_deref().unwrap_or(&node.address);
            writeln!(tw, "{}\t{}", name, version)?;
        }
    }

    tw.flush()?;
    Ok(())
}

fn run_status(
    discover_all: bool,
    targets: Vec<String>,
    config: &Config,
) -> Result<(), Box<dyn Error>> {
    let multi = MultiProgress::new();
    let native = Arc::new(NativeProgress::new());
    let native_supported = native.is_supported();
    let spinner_style = ProgressStyle::default_spinner()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈")
        .template("{spinner:.green} {msg}")?;

    let (target_tx, target_rx) = flume::unbounded::<(String, Option<String>)>();

    let active_profile = config.profiles.get(&config.active_profile);

    // 1. Collect explicit targets and profile nodes
    let mut initial_targets = HashSet::new();
    for t in targets {
        let mut name = None;
        let mut addr = t.clone();
        if let Some(profile) = active_profile {
            if let Some(node) = profile
                .nodes
                .iter()
                .find(|n| n.address == t || n.name.as_ref() == Some(&t))
            {
                addr = node.address.clone();
                name = node.name.clone();
            }
        }
        initial_targets.insert((addr, name));
    }

    if initial_targets.is_empty() && !discover_all {
        if let Some(profile) = active_profile {
            for node in &profile.nodes {
                initial_targets.insert((node.address.clone(), node.name.clone()));
            }
        }
    }

    for t in initial_targets {
        target_tx.send(t)?;
    }

    // 2. Start discovery in background if requested
    if discover_all {
        native.set_indeterminate();
        let pb = if native_supported {
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
            let timeout = Duration::from_secs(5);
            if let Ok(targets) = discover_targets_with_timeout(timeout) {
                for (addr, name) in targets {
                    let _ = tx.send((addr, Some(name)));
                }
            }
            pb.finish_and_clear();
        });
    }

    drop(target_tx); // Close tx so rx finishes when discovery is done

    let client = ca::build_client()?;

    let seen_targets = Arc::new(Mutex::new(HashSet::new()));
    let node_reports = Arc::new(Mutex::new(Vec::<(String, bool, bool)>::new()));

    let pb = if native_supported {
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

    let any_failed = Arc::new(AtomicBool::new(false));

    rayon::scope(|s| {
        while let Ok((target_addr, target_name)) = target_rx.recv() {
            let client = client.clone();
            let seen_targets = Arc::clone(&seen_targets);
            let node_reports = Arc::clone(&node_reports);
            let pb = pb.clone();
            let multi = multi.clone();
            let native = Arc::clone(&native);
            let any_failed = Arc::clone(&any_failed);

            pb.inc_length(1);
            native.set_progress(pb.position(), pb.length());

            s.spawn(move |_| {
                if !seen_targets.lock().unwrap().insert(target_addr.clone()) {
                    pb.inc_length(0); // Adjust length if skipped
                    pb.inc(1);
                    native.set_progress(pb.position(), pb.length());
                    return;
                }

                let (status, message, details, failed, outdated) =
                    query_status(&client, &target_addr);

                if failed {
                    any_failed.store(true, Ordering::Relaxed);
                }

                let mut output = String::new();
                let name = target_name.as_deref().unwrap_or(&target_addr);
                let _ = writeln!(output, "Node: {}", name);
                let _ = writeln!(output, "Address: {}", target_addr);
                if !message.is_empty() {
                    let _ = writeln!(output, "{}", message);
                }
                let _ = writeln!(output, "Details:");
                let _ = writeln!(output, "    Status: {}", status);
                if !details.is_empty() {
                    let _ = writeln!(output, "    {}", details.trim().replace('\n', "\n    "));
                }
                let _ = writeln!(output);
                let _ = multi.println(output);

                node_reports
                    .lock()
                    .unwrap()
                    .push((name.to_string(), failed, outdated));
                pb.inc(1);
                native.set_progress(pb.position(), pb.length());
            });
        }
    });

    pb.finish_and_clear();

    let reports = node_reports.lock().unwrap();
    if !reports.is_empty() {
        println!("Summary:");
        let outdated_nodes: Vec<_> = reports
            .iter()
            .filter(|(_, _, outdated)| *outdated)
            .map(|(name, _, _)| name.as_str())
            .collect();

        if outdated_nodes.is_empty() {
            println!("- All nodes are up to date.");
        } else {
            println!(
                "- {} node(s) are outdated: {}",
                outdated_nodes.len(),
                outdated_nodes.join(", ")
            );
        }

        let failed_nodes: Vec<_> = reports
            .iter()
            .filter(|(_, failed, _)| *failed)
            .map(|(name, _, _)| name.as_str())
            .collect();

        if !failed_nodes.is_empty() {
            println!(
                "- {} node(s) failed to respond: {}",
                failed_nodes.len(),
                failed_nodes.join(", ")
            );
        }
    }

    if native_supported {
        if any_failed.load(Ordering::Relaxed) {
            native.finish_error();
        } else {
            native.finish_success();
        }
        std::thread::sleep(Duration::from_millis(500));
        native.clear();
    }

    Ok(())
}

fn query_status(
    client: &reqwest::blocking::Client,
    target: &str,
) -> (String, String, String, bool, bool) {
    let url = resolve_url(target);
    let status_url = format!("{}{}", url, PATH_STATUS);

    match client.get(&status_url).send() {
        Ok(resp) => {
            let status = resp.status().to_string();
            if resp.status().is_success() {
                match resp.json::<StatusResponse>() {
                    Ok(sr) => {
                        let message = sr.message;
                        let mut details = String::new();
                        if let Some(version) = sr.daemon_version {
                            details.push_str(&format!("Daemon version: {}\n", version));
                        }
                        details.push_str(&format!("Upgrading: {}\n", sr.is_upgrading));
                        details.push_str(&format!("Allow reboot: {}\n", sr.allow_reboot));
                        details.push_str(&format!("Auto clean: {}\n", sr.auto_clean));
                        details.push_str(&format!("Auto remove: {}\n", sr.auto_remove));
                        if !sr.updates.is_empty() {
                            details.push_str("Updates:\n");
                            for update in &sr.updates {
                                details.push_str(&format!("  - {}\n", update));
                            }
                        } else {
                            details.push_str("No updates available.\n");
                        }
                        let outdated = !sr.updates.is_empty();
                        (status, message, details, false, outdated)
                    }
                    Err(_) => (
                        status,
                        "Could not parse StatusResponse".to_string(),
                        "".to_string(),
                        false,
                        false,
                    ),
                }
            } else {
                match resp.json::<serde_json::Value>() {
                    Ok(json) => {
                        let details = serde_json::to_string_pretty(&json)
                            .unwrap_or_else(|_| "Failed to pretty-print JSON".to_string());
                        (status, "".to_string(), details, false, false)
                    }
                    Err(_) => (
                        status,
                        "Could not parse response as JSON".to_string(),
                        "".to_string(),
                        false,
                        false,
                    ),
                }
            }
        }
        Err(err) => (
            format!("Error: {}", err),
            "".to_string(),
            "".to_string(),
            true,
            false,
        ),
    }
}

fn discover_targets_with_timeout(
    timeout: Duration,
) -> Result<Vec<(String, String)>, Box<dyn Error>> {
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
            Err(err) => return Err(format!("mDNS error: {err}").into()),
        }
    }
    let _ = mdns.shutdown();
    Ok(targets)
}

fn resolve_url(target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        target.trim_end_matches('/').to_string()
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

fn run_node(
    full_upgrade: bool,
    reboot: bool,
    dry_run: bool,
    list: bool,
    nodes: Vec<String>,
    config: &Config,
) -> Result<(), Box<dyn Error>> {
    if !full_upgrade && !reboot && !list {
        return Err(
            "either --full-upgrade (-u), --reboot (-r), or --list (-l) must be specified".into(),
        );
    }

    if dry_run && !full_upgrade {
        return Err("--dry-run can only be used with --full-upgrade".into());
    }

    let target_nodes = resolve_profile_nodes(&nodes, config)?;

    if target_nodes.is_empty() {
        println!("No nodes found.");
        return Ok(());
    }

    if list {
        for node in target_nodes {
            let name = node.name.as_deref().unwrap_or(&node.address);
            println!("{}", name);
        }
        return Ok(());
    }

    let client = ca::build_client()?;

    let mut tw = TabWriter::new(io::stdout());
    writeln!(tw, "NODE\tSTATUS")?;

    for node in target_nodes {
        let name = node.name.as_deref().unwrap_or(&node.address);
        let url = resolve_url(&node.address);

        let (status, body, updates) = if reboot {
            let reboot_url = format!("{}{}", url, PATH_REBOOT);
            let request = client
                .post(&reboot_url)
                .json(&brewmble_rest::RebootRequest::default());

            match request.send() {
                Ok(resp) => {
                    let status = resp.status().to_string();
                    let rr: RebootResponse = resp.json().unwrap_or_else(|_| RebootResponse {
                        message: "Unknown response".to_string(),
                    });
                    (status, rr.message, None)
                }
                Err(err) => (format!("Error: {}", err), "".to_string(), None),
            }
        } else {
            let upgrade_url = format!("{}{}", url, PATH_UPGRADE);
            let request = client
                .post(&upgrade_url)
                .json(&brewmble_rest::UpgradeRequest { dry_run });

            match request.send() {
                Ok(resp) => {
                    let status = resp.status().to_string();
                    let ur: UpgradeResponse = resp.json().unwrap_or_else(|_| UpgradeResponse {
                        message: "Unknown response".to_string(),
                        updates: None,
                    });
                    (status, ur.message, ur.updates)
                }
                Err(err) => (format!("Error: {}", err), "".to_string(), None),
            }
        };

        writeln!(tw, "{}\t{}", name, status)?;
        if !body.is_empty() {
            writeln!(tw, "\t{}", body.replace('\n', "\n\t"))?;
        }
        if let Some(upds) = updates {
            if !upds.is_empty() {
                writeln!(tw, "\tUpdates ({}): {}", upds.len(), upds.join(", "))?;
            }
        }
    }

    tw.flush()?;

    Ok(())
}
