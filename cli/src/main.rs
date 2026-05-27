use cobbler_rest::StatusResponse;
use clap::{Parser, Subcommand};
use flume::RecvTimeoutError;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tabwriter::TabWriter;

const SERVICE_TYPE: &str = "_cobbler._tcp";
const SERVICE_DOMAIN: &str = "local.";
const TOKEN_PLACEHOLDER: &str = "REPLACE_WITH_ACTUAL_TOKEN";

#[derive(Serialize, Deserialize, Default, Debug)]
struct Config {
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
    let config = serde_yaml::from_str(&content)?;
    Ok(config)
}

fn save_config(path: &Path, config: &Config) -> Result<(), Box<dyn Error>> {
    let content = serde_yaml::to_string(config)?;
    fs::write(path, content)?;
    Ok(())
}

fn merge_nodes(config: &mut Config, discovered: Vec<(String, String)>) -> bool {
    let mut updated = false;
    for (addr, id) in discovered {
        let new_name = if id.is_empty() { None } else { Some(id) };

        // Try finding by name first if name is available
        let mut found_index = None;
        if let Some(ref name) = new_name {
            found_index = config.nodes.iter().position(|n| n.name.as_ref() == Some(name));
        }

        // If not found by name, try finding by address
        if found_index.is_none() {
            found_index = config.nodes.iter().position(|n| n.address == addr);
        }

        if let Some(index) = found_index {
            let node = &mut config.nodes[index];
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
            config.nodes.push(NodeConfig {
                name: new_name,
                address: addr,
                api_key: Some(TOKEN_PLACEHOLDER.to_string()),
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

        /// Targets (host:port)
        #[arg(num_args = 0..)]
        targets: Vec<String>,
    },
}

fn main() {
    let cli = Cli::parse();
    let (config_path, config_exists) = resolve_config_path(cli.config);
    let config = match load_config(&config_path) {
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
            targets,
        } => {
            if targets.is_empty() && !config_exists {
                println!("No config file was found or set.");
            }
            run_packages(full_upgrade, targets, &config)
        }
    };

    if let Err(err) = result {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
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
        let mut config = Config {
            nodes: vec![NodeConfig {
                name: None,
                address: "1.1.1.1:8080".to_string(),
                api_key: None,
            }],
        };

        let discovered = vec![
            ("1.1.1.1:8080".to_string(), "node1".to_string()),
            ("2.2.2.2:8080".to_string(), "node2".to_string()),
        ];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        assert_eq!(config.nodes.len(), 2);
        
        // Existing node updated with name
        assert_eq!(config.nodes[0].address, "1.1.1.1:8080");
        assert_eq!(config.nodes[0].name, Some("node1".to_string()));
        assert_eq!(config.nodes[0].api_key, None);

        // New node added with name and placeholder token
        assert_eq!(config.nodes[1].address, "2.2.2.2:8080");
        assert_eq!(config.nodes[1].name, Some("node2".to_string()));
        assert_eq!(config.nodes[1].api_key, Some(TOKEN_PLACEHOLDER.to_string()));
    }

    #[test]
    fn test_merge_nodes_updates_name_but_preserves_token() {
        let mut config = Config {
            nodes: vec![NodeConfig {
                name: Some("OldName".to_string()),
                address: "1.1.1.1:8080".to_string(),
                api_key: Some("secret".to_string()),
            }],
        };

        let discovered = vec![("1.1.1.1:8080".to_string(), "NewName".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        assert_eq!(config.nodes[0].name, Some("NewName".to_string()));
        assert_eq!(config.nodes[0].api_key, Some("secret".to_string()));
    }

    #[test]
    fn test_merge_nodes_updates_custom_name() {
        let mut config = Config {
            nodes: vec![NodeConfig {
                name: Some("Custom".to_string()),
                address: "1.1.1.1:8080".to_string(),
                api_key: None,
            }],
        };

        let discovered = vec![("1.1.1.1:8080".to_string(), "node1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        assert_eq!(config.nodes[0].name, Some("node1".to_string()));
    }

    #[test]
    fn test_merge_nodes_cleans_id_prefix_from_config() {
        let mut config = Config {
            nodes: vec![NodeConfig {
                name: Some("id=raspi1".to_string()),
                address: "1.1.1.1:8080".to_string(),
                api_key: None,
            }],
        };

        // Discovered node has the clean name
        let discovered = vec![("1.1.1.1:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        assert_eq!(config.nodes[0].name, Some("raspi1".to_string()));
    }

    #[test]
    fn test_merge_nodes_prevents_duplicate_by_name() {
        let mut config = Config {
            nodes: vec![NodeConfig {
                name: Some("raspi1".to_string()),
                address: "1.1.1.1:8080".to_string(),
                api_key: Some("secret".to_string()),
            }],
        };

        // raspi1 changed IP
        let discovered = vec![("1.1.1.2:8080".to_string(), "raspi1".to_string())];

        let updated = merge_nodes(&mut config, discovered);
        assert!(updated);
        assert_eq!(config.nodes.len(), 1);
        assert_eq!(config.nodes[0].address, "1.1.1.2:8080");
        assert_eq!(config.nodes[0].name, Some("raspi1".to_string()));
        assert_eq!(config.nodes[0].api_key, Some("secret".to_string()));
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

    if targets.is_empty() {
        for node in &config.nodes {
            targets.push(node.address.clone());
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
        let status_url = format!("{}/status", url);

        let mut request = client.get(&status_url);

        if let Some(node) = config.nodes.iter().find(|n| n.address == target) {
            if let Some(api_key) = &node.api_key {
                request = request.header("X-API-Key", api_key);
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
    mut targets: Vec<String>,
    config: &Config,
) -> Result<(), Box<dyn Error>> {
    if targets.is_empty() {
        for node in &config.nodes {
            targets.push(node.address.clone());
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
        let upgrade_url = format!("{}/packages/full-upgrade", url);

        let mut request = client.post(&upgrade_url);

        if let Some(node) = config.nodes.iter().find(|n| n.address == target) {
            if let Some(api_key) = &node.api_key {
                request = request.header("X-API-Key", api_key);
            }
        }

        let (status, body) = match request.send() {
            Ok(resp) => {
                let status = resp.status().to_string();
                let body = match resp.json::<serde_json::Value>() {
                    Ok(json) => serde_json::to_string_pretty(&json)
                        .unwrap_or_else(|_| "Failed to pretty-print JSON".to_string()),
                    Err(_) => "Upgrade triggered successfully".to_string(),
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

