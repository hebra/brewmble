use cobbler_rest::{
    StatusResponse, UpgradeResponse, API_KEY_HEADER, PATH_STATUS, PATH_UPGRADE, SERVICE_DOMAIN,
    SERVICE_TYPE,
};
use clap::{Parser, Subcommand};
use flume::RecvTimeoutError;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use keyring::Entry;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tabwriter::TabWriter;

const TOKEN_PLACEHOLDER: &str = "REPLACE_WITH_ACTUAL_TOKEN";

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
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    #[serde(default)]
    use_keyring: bool,
}

impl NodeConfig {
    fn get_api_key(&self) -> Option<String> {
        if self.use_keyring {
            let entry = Entry::new("cobbler-cli", &self.address).ok()?;
            entry.get_password().ok()
        } else {
            self.api_key.clone()
        }
    }
}

fn resolve_config_path(explicit_path: Option<PathBuf>) -> (PathBuf, bool) {
    if let Some(path) = explicit_path {
        return (path, true);
    }

    if let Some(mut home) = home::home_dir() {
        home.push(".cobbler.yaml");
        if home.exists() {
            return (home, true);
        }
    }

    let local_path = PathBuf::from(".cobbler.yaml");
    if local_path.exists() {
        (local_path, true)
    } else {
        // Default to home dir if it exists
        if let Some(mut home) = home::home_dir() {
            home.push(".cobbler.yaml");
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
                api_key: Some(TOKEN_PLACEHOLDER.to_string()),
                use_keyring: false,
            });
            updated = true;
        }
    }
    updated
}

fn get_default_timeout() -> Duration {
    std::env::var("COBBLER_TIMEOUT")
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
#[command(name = "cobbler")]
#[command(about = "A CLI tool for cobbler", long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long, env = "COBBLER_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Discover cobbler daemons on the local network
    Discover {
        /// Time to wait for responses in seconds
        #[arg(short, long, default_value = "5", env = "COBBLER_TIMEOUT")]
        timeout: u64,

        /// Create and/or update a config file with newly found daemons
        #[arg(short = 'u', long = "update-config")]
        update_config: bool,
    },
    /// Show status of cobbler daemons
    Status {
        /// Get status for all discovered cobbler daemons
        #[arg(short, long)]
        all: bool,

        /// Targets (host:port)
        targets: Vec<String>,
    },
    /// Manage packages on cobbler daemons
    Packages {
        /// Perform a full system upgrade
        #[arg(long, required = true)]
        full_upgrade: bool,

        /// Show what would be upgraded without executing
        #[arg(long)]
        dry_run: bool,

        /// Targets (host:port)
        #[arg(num_args = 0..)]
        targets: Vec<String>,
    },
    /// Manage configuration profiles
    Profile {
        #[command(subcommand)]
        subcommand: ProfileCommands,
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
    /// Set API key for a node in the keyring
    SetKey {
        /// Profile name (defaults to active profile)
        #[arg(short, long)]
        profile: Option<String>,
        /// Node address or name
        node: String,
        /// API key
        key: String,
    },
}

fn main() {
    let cli = Cli::parse();
    let (config_path, config_exists) = resolve_config_path(cli.config);
    let mut config = match load_config(&config_path) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("error: failed to load config: {err}");
            std::process::exit(1);
        }
    };

    let result = match cli.command {
        Commands::Discover {
            timeout,
            update_config,
        } => run_discover(Duration::from_secs(timeout), update_config, &config_path),
        Commands::Status { all, targets } => {
            if targets.is_empty() && !all && !config_exists {
                println!("No config file was found or set.");
            }
            run_status(all, targets, &config)
        }
        Commands::Packages {
            full_upgrade,
            dry_run,
            targets,
        } => {
            if targets.is_empty() && !config_exists {
                println!("No config file was found or set.");
            }
            run_packages(full_upgrade, dry_run, targets, &config)
        }
        Commands::Profile { subcommand } => run_profile(subcommand, &mut config, &config_path),
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
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
            config.profiles.insert(name.clone(), ProfileConfig::default());
            save_config(config_path, config)?;
            println!("Created profile '{}'", name);
        }
        ProfileCommands::Delete { name } => {
            if name == default_profile_name() {
                return Err("Cannot delete the default profile".into());
            }
            if name == config.active_profile {
                return Err("Cannot delete the active profile. Switch to another profile first.".into());
            }
            if config.profiles.remove(&name).is_some() {
                save_config(config_path, config)?;
                println!("Deleted profile '{}'", name);
            } else {
                return Err(format!("Profile '{}' not found", name).into());
            }
        }
        ProfileCommands::SetKey {
            profile: profile_name,
            node: node_id,
            key,
        } => {
            let p_name = profile_name.unwrap_or_else(|| config.active_profile.clone());
            {
                let profile = config
                    .profiles
                    .get_mut(&p_name)
                    .ok_or_else(|| format!("Profile '{}' not found", p_name))?;

                let node = profile
                    .nodes
                    .iter_mut()
                    .find(|n| n.address == node_id || n.name.as_ref() == Some(&node_id))
                    .ok_or_else(|| format!("Node '{}' not found in profile '{}'", node_id, p_name))?;

                let entry = Entry::new("cobbler-cli", &node.address)?;
                entry.set_password(&key)?;

                node.use_keyring = true;
                node.api_key = None; // Remove plain-text key if it was there
            }

            save_config(config_path, config)?;
            println!(
                "API key for node stored securely in keyring for profile '{}'",
                p_name
            );
        }
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
    let service_name = format!(
        "{}.{}",
        SERVICE_TYPE.trim_end_matches('.'),
        SERVICE_DOMAIN
    );
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
                            writeln!(writer, "ID\tHOST\tADDRESS\tPORT\tINSTANCE")?;
                            header_printed = true;
                        }
                        writeln!(
                            writer,
                            "{}\t{}\t{}\t{}\t{}",
                            entry_id(&info),
                            entry_host(&info),
                            entry_addresses(&info),
                            info.get_port(),
                            entry_instance(&info)
                        )?;
                        writer.flush()?;

                        if let Some(addr) = info.get_addresses().iter().next() {
                            discovered_nodes.push((
                                format!("{}:{}", addr, info.get_port()),
                                entry_id(&info),
                            ));
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
        println!("No cobbler daemons found.");
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

    #[test]
    fn test_cli_parse_discover_default() {
        let cli = Cli::parse_from(&["cobbler", "discover"]);
        if let Commands::Discover {
            timeout,
            update_config,
        } = cli.command
        {
            assert_eq!(timeout, 5);
            assert!(!update_config);
        } else {
            panic!("Wrong command");
        }
    }

    #[test]
    fn test_cli_parse_discover_timeout() {
        let cli = Cli::parse_from(&["cobbler", "discover", "-t", "10", "-u"]);
        if let Commands::Discover {
            timeout,
            update_config,
        } = cli.command
        {
            assert_eq!(timeout, 10);
            assert!(update_config);
        } else {
            panic!("Wrong command");
        }
    }

    #[test]
    fn test_cli_parse_packages_dry_run() {
        let cli = Cli::parse_from(&["cobbler", "packages", "--full-upgrade", "--dry-run"]);
        if let Commands::Packages {
            full_upgrade,
            dry_run,
            targets,
        } = cli.command
        {
            assert!(full_upgrade);
            assert!(dry_run);
            assert!(targets.is_empty());
        } else {
            panic!("Expected Packages command");
        }
    }

    #[test]
    fn test_cli_parse_packages_no_dry_run() {
        let cli = Cli::parse_from(&["cobbler", "packages", "--full-upgrade", "host:8080"]);
        if let Commands::Packages {
            full_upgrade,
            dry_run,
            targets,
        } = cli.command
        {
            assert!(full_upgrade);
            assert!(!dry_run);
            assert_eq!(targets, vec!["host:8080"]);
        } else {
            panic!("Expected Packages command");
        }
    }

    #[test]
    fn test_resolve_config_path() {
        let explicit = Some(PathBuf::from("custom.yaml"));
        let (path, exists) = resolve_config_path(explicit);
        assert_eq!(path, PathBuf::from("custom.yaml"));
        assert!(exists);

        let (path, _) = resolve_config_path(None);
        assert_eq!(path, PathBuf::from(".cobbler.yaml"));
    }

    #[test]
    fn test_get_default_timeout() {
        std::env::set_var("COBBLER_TIMEOUT", "15");
        assert_eq!(get_default_timeout(), Duration::from_secs(15));

        std::env::set_var("COBBLER_TIMEOUT", "1m");
        assert_eq!(get_default_timeout(), Duration::from_secs(60));

        std::env::remove_var("COBBLER_TIMEOUT");
        assert_eq!(get_default_timeout(), Duration::from_secs(60));
    }

    #[test]
    fn test_merge_nodes() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: None,
            address: "1.1.1.1:8080".to_string(),
            api_key: None,
            use_keyring: false,
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
        assert_eq!(nodes[0].api_key, None);

        // New node added with name and placeholder token
        assert_eq!(nodes[1].address, "2.2.2.2:8080");
        assert_eq!(nodes[1].name, Some("node2".to_string()));
        assert_eq!(nodes[1].api_key, Some(TOKEN_PLACEHOLDER.to_string()));
    }

    #[test]
    fn test_merge_nodes_updates_name_but_preserves_token() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("OldName".to_string()),
            address: "1.1.1.1:8080".to_string(),
            api_key: Some("secret".to_string()),
            use_keyring: false,
        }];

        let discovered = vec![("1.1.1.1:8080".to_string(), "NewName".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes[0].name, Some("NewName".to_string()));
        assert_eq!(nodes[0].api_key, Some("secret".to_string()));
    }

    #[test]
    fn test_merge_nodes_updates_custom_name() {
        let mut config = Config::default();
        config.profiles.get_mut("default").unwrap().nodes = vec![NodeConfig {
            name: Some("Custom".to_string()),
            address: "1.1.1.1:8080".to_string(),
            api_key: None,
            use_keyring: false,
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
            api_key: None,
            use_keyring: false,
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
            api_key: Some("secret".to_string()),
            use_keyring: false,
        }];

        // raspi1 changed IP
        let discovered = vec![("1.1.1.2:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        let nodes = &config.profiles.get("default").unwrap().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].address, "1.1.1.2:8080");
        assert_eq!(nodes[0].name, Some("raspi1".to_string()));
        assert_eq!(nodes[0].api_key, Some("secret".to_string()));
    }

    #[test]
    fn test_clean_node_id() {
        assert_eq!(clean_node_id("id=raspi1"), "raspi1");
        assert_eq!(clean_node_id("raspi1"), "raspi1");
        assert_eq!(clean_node_id(""), "");
    }

    #[test]
    fn test_resolve_url() {
        assert_eq!(resolve_url("1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(resolve_url("http://1.2.3.4:8080"), "http://1.2.3.4:8080");
        assert_eq!(resolve_url("https://example.com"), "https://example.com");
        assert_eq!(resolve_url("example.com:80"), "http://example.com:80");
        assert_eq!(resolve_url("::1:8080"), "http://[::1]:8080");
        assert_eq!(resolve_url("[::1]:8080"), "http://[::1]:8080");
        assert_eq!(resolve_url("localhost"), "http://localhost");
        assert_eq!(resolve_url("1.2.3.4:8080/"), "http://1.2.3.4:8080");
    }

    #[test]
    fn test_entry_helpers() {
        let properties = [("id", "node1")];
        let info = ServiceInfo::new(
            "_cobbler._tcp.local.",
            "cobblerd-node1",
            "node1.local.",
            "1.2.3.4",
            8080,
            &properties[..],
        ).unwrap();

        assert_eq!(entry_id(&info), "node1");
        assert_eq!(entry_host(&info), "node1.local");
        assert_eq!(entry_addresses(&info), "1.2.3.4");
        assert_eq!(entry_instance(&info), "cobblerd-node1");
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
        assert_eq!(profile.nodes[0].api_key, Some("secret-token".to_string()));
    }
}


fn clean_node_id(id: &str) -> &str {
    id.strip_prefix("id=").unwrap_or(id)
}

fn entry_id(entry: &ServiceInfo) -> String {
    let props = entry.get_properties();
    props
        .get("id")
        .map(|value| clean_node_id(&value.to_string()).to_string())
        .unwrap_or_default()
}

fn entry_host(entry: &ServiceInfo) -> String {
    entry.get_hostname().trim_end_matches('.').to_string()
}

fn entry_addresses(entry: &ServiceInfo) -> String {
    let mut parts = Vec::new();
    let addrs = entry.get_addresses();
    for addr in addrs.iter().filter(|addr| addr.is_ipv4()) {
        parts.push(addr.to_string());
    }
    for addr in addrs.iter().filter(|addr| addr.is_ipv6()) {
        parts.push(addr.to_string());
    }
    parts.join(",")
}

fn entry_instance(entry: &ServiceInfo) -> String {
    let fullname = entry.get_fullname();
    let suffix = format!(
        ".{}.{}",
        SERVICE_TYPE.trim_end_matches('.'),
        SERVICE_DOMAIN
    );
    fullname
        .strip_suffix(&suffix)
        .unwrap_or(fullname)
        .to_string()
}

fn run_status(
    discover_all: bool,
    mut targets: Vec<String>,
    config: &Config,
) -> Result<(), Box<dyn Error>> {
    if discover_all {
        targets.extend(discover_targets()?);
    }

    let active_profile = config.profiles.get(&config.active_profile);

    if targets.is_empty() {
        if let Some(profile) = active_profile {
            for node in &profile.nodes {
                targets.push(node.address.clone());
            }
        }
    }

    if targets.is_empty() {
        println!("No targets found.");
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(get_default_timeout())
        .build()?;

    let mut tw = TabWriter::new(io::stdout());
    writeln!(tw, "TARGET\tSTATUS")?;

    for target in targets {
        let url = resolve_url(&target);
        let status_url = format!("{}{}", url, PATH_STATUS);

        let mut request = client.get(&status_url);

        if let Some(profile) = active_profile {
            if let Some(node) = profile
                .nodes
                .iter()
                .find(|n| n.address == target || n.name.as_ref() == Some(&target))
            {
                if let Some(api_key) = node.get_api_key() {
                    request = request.header(API_KEY_HEADER, api_key);
                }
            }
        }

        let (status, body) = match request.send() {
            Ok(resp) => {
                let status = resp.status().to_string();
                let body = if resp.status().is_success() {
                    match resp.json::<StatusResponse>() {
                        Ok(sr) => {
                            let mut s = format!("Message: {}\n", sr.message);
                            s.push_str(&format!("Upgrading: {}\n", sr.is_upgrading));
                            if !sr.updates.is_empty() {
                                s.push_str("Updates:\n");
                                for update in &sr.updates {
                                    s.push_str(&format!("  - {}\n", update));
                                }
                            } else {
                                s.push_str("No updates available.\n");
                            }
                            s
                        }
                        Err(_) => "Could not parse StatusResponse".to_string(),
                    }
                } else {
                    match resp.json::<serde_json::Value>() {
                        Ok(json) => serde_json::to_string_pretty(&json)
                            .unwrap_or_else(|_| "Failed to pretty-print JSON".to_string()),
                        Err(_) => "Could not parse response as JSON".to_string(),
                    }
                };
                (status, body)
            }
            Err(err) => (format!("Error: {}", err), "".to_string()),
        };

        writeln!(tw, "{}\t{}", target, status)?;
        if !body.is_empty() {
            writeln!(tw, "\t{}", body.replace('\n', "\n\t"))?;
        }
    }

    tw.flush()?;

    Ok(())
}

fn discover_targets() -> Result<Vec<String>, Box<dyn Error>> {
    let mut targets = Vec::new();
    let mdns = ServiceDaemon::new().map_err(|err| format!("create resolver: {err}"))?;
    let service_name = format!("{}.{}", SERVICE_TYPE.trim_end_matches('.'), SERVICE_DOMAIN);
    let receiver = mdns
        .browse(&service_name)
        .map_err(|err| format!("browse: {err}"))?;

    let timeout = get_default_timeout();
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
                    for addr in info.get_addresses() {
                        let target = format!("{}:{}", addr, info.get_port());
                        if seen.insert(target.clone()) {
                            targets.push(target);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => break,
            Err(err) => return Err(format!("mDNS error: {err}").into()),
        }
    }
    Ok(targets)
}

fn resolve_url(target: &str) -> String {
    if target.starts_with("http://") || target.starts_with("https://") {
        target.trim_end_matches('/').to_string()
    } else if target.contains(':') && target.split(':').last().unwrap().chars().all(|c| c.is_ascii_digit()) {
        let parts: Vec<&str> = target.split(':').collect();
        let host = parts[..parts.len() - 1].join(":");
        let port = parts.last().unwrap();

        if host.contains(':') && !host.starts_with('[') {
            format!("http://[{}]:{}", host, port)
        } else {
            format!("http://{}:{}", host, port)
        }
    } else {
        format!("http://{}", target.trim_end_matches('/'))
    }
}


fn run_packages(
    _full_upgrade: bool,
    dry_run: bool,
    mut targets: Vec<String>,
    config: &Config,
) -> Result<(), Box<dyn Error>> {
    let active_profile = config.profiles.get(&config.active_profile);

    if targets.is_empty() {
        if let Some(profile) = active_profile {
            for node in &profile.nodes {
                targets.push(node.address.clone());
            }
        }
    }

    if targets.is_empty() {
        println!("No targets found.");
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(get_default_timeout())
        .build()?;

    let mut tw = TabWriter::new(io::stdout());
    writeln!(tw, "TARGET\tSTATUS")?;

    for target in targets {
        let url = resolve_url(&target);
        let upgrade_url = format!("{}{}", url, PATH_UPGRADE);

        let mut request = client
            .post(&upgrade_url)
            .json(&cobbler_rest::UpgradeRequest { dry_run });

        if let Some(profile) = active_profile {
            if let Some(node) = profile
                .nodes
                .iter()
                .find(|n| n.address == target || n.name.as_ref() == Some(&target))
            {
                if let Some(api_key) = node.get_api_key() {
                    request = request.header(API_KEY_HEADER, api_key);
                }
            }
        }

        let (status, body, updates) = match request.send() {
            Ok(resp) => {
                let status = resp.status().to_string();
                let ur: UpgradeResponse = resp.json().unwrap_or_else(|_| UpgradeResponse {
                    message: "Unknown response".to_string(),
                    updates: None,
                });
                (status, ur.message, ur.updates)
            }
            Err(err) => (format!("Error: {}", err), "".to_string(), None),
        };

        writeln!(tw, "{}\t{}", target, status)?;
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

