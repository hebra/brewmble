use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

pub const DEFAULT_UPDATE_INTERVAL_MINS: u64 = 360;

fn default_update_interval_mins() -> u64 {
    DEFAULT_UPDATE_INTERVAL_MINS
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    #[serde(default)]
    pub server: ServerConfig,
    pub tls: TlsConfig,
    #[serde(default)]
    pub features: FeaturesConfig,
    #[serde(default)]
    pub package_manager: PackageManagerConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<IpAddr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<IpAddr>,
    #[serde(default = "default_true")]
    pub mdns: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: None,
            hostname: None,
            ip: None,
            bind_address: None,
            mdns: default_true(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct TlsConfig {
    pub ca_cert: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct FeaturesConfig {
    #[serde(default)]
    pub allow_reboot: bool,
    #[serde(default)]
    pub auto_clean: bool,
    #[serde(default)]
    pub auto_remove: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct PackageManagerConfig {
    #[serde(default = "default_update_interval_mins")]
    pub update_interval_mins: u64,
}

impl Default for PackageManagerConfig {
    fn default() -> Self {
        Self {
            update_interval_mins: default_update_interval_mins(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug)]
pub enum ConfigError {
    NotFound { searched_paths: Vec<PathBuf> },
    ExplicitFileNotFound(PathBuf),
    Io(std::io::Error),
    Parse(serde_yaml_ng::Error),
    Validation(Vec<String>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NotFound { searched_paths } => {
                write!(
                    f,
                    "No configuration file found in search locations:\n{}\nRun 'brewmbled config init' to create a template configuration.",
                    searched_paths
                        .iter()
                        .map(|p| format!("  - {}", p.display()))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            ConfigError::ExplicitFileNotFound(path) => {
                write!(f, "Configuration file not found: {}", path.display())
            }
            ConfigError::Io(err) => write!(f, "Failed to read configuration file: {err}"),
            ConfigError::Parse(err) => write!(f, "Failed to parse YAML configuration: {err}"),
            ConfigError::Validation(errors) => {
                write!(
                    f,
                    "Configuration validation failed:\n{}",
                    errors
                        .iter()
                        .map(|e| format!("  - {e}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Parse(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::Io(err)
    }
}

impl From<serde_yaml_ng::Error> for ConfigError {
    fn from(err: serde_yaml_ng::Error) -> Self {
        ConfigError::Parse(err)
    }
}

pub fn search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(mut home) = std::env::home_dir() {
        home.push(".config");
        home.push("brewmble");
        home.push("brewmbled.yaml");
        paths.push(home);
    }
    paths.push(PathBuf::from("/etc/brewmble/brewmbled.yaml"));
    paths.push(PathBuf::from("./brewmbled.yaml"));
    paths
}

pub fn default_config_path() -> PathBuf {
    if let Some(mut home) = std::env::home_dir() {
        home.push(".config");
        home.push("brewmble");
        home.push("brewmbled.yaml");
        home
    } else {
        PathBuf::from("/etc/brewmble/brewmbled.yaml")
    }
}

pub fn resolve_config_path(explicit_path: Option<&Path>) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit_path {
        if path.exists() {
            return Ok(path.to_path_buf());
        } else {
            return Err(ConfigError::ExplicitFileNotFound(path.to_path_buf()));
        }
    }

    let candidates = search_paths();
    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    Err(ConfigError::NotFound {
        searched_paths: candidates,
    })
}

pub fn load_config(path: &Path) -> Result<DaemonConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
    let config: DaemonConfig = serde_yaml_ng::from_str(&content)?;
    Ok(config)
}

pub fn default_config_template() -> &'static str {
    r#"# brewmble daemon configuration file
# Generated by `brewmbled config init`

server:
  # Port to listen on. If omitted, the daemon searches for a free port starting at 4712.
  # port: 4712

  # Hostname to announce via mDNS. Defaults to the system hostname.
  # hostname: "node-1"

  # Explicit IP address to announce via mDNS. Defaults to auto-discovered IP.
  # ip: "192.168.1.10"

  # Local address to bind the HTTPS listener to. Defaults to 0.0.0.0 (all interfaces).
  # bind_address: "0.0.0.0"

  # Enable mDNS service discovery. Defaults to true.
  # mdns: true

tls:
  # Path to the cluster CA certificate PEM file (required)
  ca_cert: "/etc/brewmble/tls/ca.pem"

  # Path to the daemon certificate PEM file (required)
  cert: "/etc/brewmble/tls/daemon.pem"

  # Path to the daemon private key PEM file (required)
  key: "/etc/brewmble/tls/daemon-key.pem"

features:
  # Allow the daemon to reboot the host when requested via the API (default: false)
  allow_reboot: false

  # Automatically clean downloaded packages after upgrade (default: false)
  auto_clean: false

  # Automatically remove unused packages after upgrade (default: false)
  auto_remove: false

package_manager:
  # Package manager cache update interval in minutes (default: 360)
  update_interval_mins: 360
"#
}

fn write_config_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        std::io::Write::write_all(&mut file, content.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save_config(path: &Path, config: &DaemonConfig) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
    }
    let serialized = serde_yaml_ng::to_string(config)?;
    write_config_atomically(path, &serialized).map_err(ConfigError::Io)?;
    Ok(())
}

pub fn init_config_file(path: &Path, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() && !force {
        return Err(format!(
            "Configuration file already exists at '{}'; use --force to overwrite",
            path.display()
        )
        .into());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    write_config_atomically(path, default_config_template())?;
    Ok(())
}
pub fn validate_config(config: &DaemonConfig) -> ValidationReport {
    let mut report = ValidationReport::default();

    if let Some(port) = config.server.port {
        if port == 0 {
            report.errors.push("server.port cannot be 0".to_string());
        }
    }

    // CA cert validation
    if !config.tls.ca_cert.exists() {
        report.errors.push(format!(
            "TLS CA certificate file '{}' does not exist",
            config.tls.ca_cert.display()
        ));
    } else {
        match std::fs::read_to_string(&config.tls.ca_cert) {
            Ok(pem) => {
                let ca_certs: Result<Vec<CertificateDer<'static>>, _> =
                    rustls_pemfile::certs(&mut pem.as_bytes()).collect();
                match ca_certs {
                    Ok(certs) if certs.is_empty() => {
                        report.errors.push(format!(
                            "TLS CA certificate file '{}' contains no valid certificates",
                            config.tls.ca_cert.display()
                        ));
                    }
                    Err(e) => {
                        report.errors.push(format!(
                            "TLS CA certificate file '{}' is invalid: {e}",
                            config.tls.ca_cert.display()
                        ));
                    }
                    _ => {}
                }
            }
            Err(e) => {
                report.errors.push(format!(
                    "TLS CA certificate file '{}' cannot be read: {e}",
                    config.tls.ca_cert.display()
                ));
            }
        }
    }

    // Daemon cert validation
    if !config.tls.cert.exists() {
        report.errors.push(format!(
            "TLS daemon certificate file '{}' does not exist",
            config.tls.cert.display()
        ));
    } else {
        match std::fs::read_to_string(&config.tls.cert) {
            Ok(pem) => {
                let certs: Result<Vec<CertificateDer<'static>>, _> =
                    rustls_pemfile::certs(&mut pem.as_bytes()).collect();
                match certs {
                    Ok(certs) if certs.is_empty() => {
                        report.errors.push(format!(
                            "TLS daemon certificate file '{}' contains no valid certificates",
                            config.tls.cert.display()
                        ));
                    }
                    Err(e) => {
                        report.errors.push(format!(
                            "TLS daemon certificate file '{}' is invalid: {e}",
                            config.tls.cert.display()
                        ));
                    }
                    _ => {}
                }
            }
            Err(e) => {
                report.errors.push(format!(
                    "TLS daemon certificate file '{}' cannot be read: {e}",
                    config.tls.cert.display()
                ));
            }
        }
    }

    // Daemon key validation
    if !config.tls.key.exists() {
        report.errors.push(format!(
            "TLS daemon private key file '{}' does not exist",
            config.tls.key.display()
        ));
    } else {
        match std::fs::read_to_string(&config.tls.key) {
            Ok(pem) => match rustls_pemfile::private_key(&mut pem.as_bytes()) {
                Ok(Some(_)) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(metadata) = std::fs::metadata(&config.tls.key) {
                            let mode = metadata.permissions().mode();
                            if mode & 0o077 != 0 {
                                report.warnings.push(format!(
                                        "TLS daemon private key file '{}' has permissive permissions ({:04o}); recommended is 0600",
                                        config.tls.key.display(),
                                        mode & 0o777
                                    ));
                            }
                        }
                    }
                }
                Ok(None) => {
                    report.errors.push(format!(
                        "TLS daemon private key file '{}' contains no valid private key",
                        config.tls.key.display()
                    ));
                }
                Err(e) => {
                    report.errors.push(format!(
                        "TLS daemon private key file '{}' is invalid: {e}",
                        config.tls.key.display()
                    ));
                }
            },
            Err(e) => {
                report.errors.push(format!(
                    "TLS daemon private key file '{}' cannot be read: {e}",
                    config.tls.key.display()
                ));
            }
        }
    }

    // Full TLS config load check (pair-match + parse validity)
    if report.is_valid() {
        if let Err(e) =
            crate::load_tls_config(&config.tls.ca_cert, &config.tls.cert, &config.tls.key)
        {
            report
                .errors
                .push(format!("TLS configuration is invalid: {e}"));
        }
    }

    // Expiry and EKU checks on the leaf certificate
    if report.is_valid() {
        if let Ok(pem) = std::fs::read_to_string(&config.tls.cert) {
            if let Ok((_, pem)) = x509_parser::pem::parse_x509_pem(pem.as_bytes()) {
                if let Ok(cert) = pem.parse_x509() {
                    let now = x509_parser::time::ASN1Time::now();
                    let not_after = cert.validity().not_after;
                    let not_before = cert.validity().not_before;

                    if now < not_before {
                        report.errors.push(format!(
                            "TLS daemon certificate is not yet valid (valid from {not_before})"
                        ));
                    } else if now > not_after {
                        report
                            .errors
                            .push(format!("TLS daemon certificate expired on {not_after}"));
                    } else if (not_after - now)
                        .map(|d| d < time::Duration::seconds(30 * 24 * 60 * 60))
                        .unwrap_or(false)
                    {
                        report.warnings.push(format!(
                            "TLS daemon certificate expires on {not_after} (less than 30 days)"
                        ));
                    }

                    let has_server_auth = cert
                        .extended_key_usage()
                        .ok()
                        .flatten()
                        .map(|eku| eku.value.server_auth)
                        .unwrap_or(false);
                    if !has_server_auth {
                        report.errors.push(
                            "TLS daemon certificate missing required ExtendedKeyUsage ServerAuth"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    report
}

pub fn ensure_valid_config(config: &DaemonConfig) -> Result<(), ConfigError> {
    let report = validate_config(config);
    if !report.is_valid() {
        return Err(ConfigError::Validation(report.errors));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::net::Ipv4Addr;
    use std::sync::Once;

    static INIT_CRYPTO: Once = Once::new();

    fn ensure_crypto_provider() {
        INIT_CRYPTO.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    #[test]
    fn test_deserialization_minimal() {
        let yaml = r#"
tls:
  ca_cert: /etc/brewmble/ca.pem
  cert: /etc/brewmble/daemon.pem
  key: /etc/brewmble/daemon-key.pem
"#;
        let config: DaemonConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.tls.ca_cert, PathBuf::from("/etc/brewmble/ca.pem"));
        assert_eq!(config.tls.cert, PathBuf::from("/etc/brewmble/daemon.pem"));
        assert_eq!(
            config.tls.key,
            PathBuf::from("/etc/brewmble/daemon-key.pem")
        );
        assert_eq!(config.server.port, None);
        assert_eq!(config.server.hostname, None);
        assert_eq!(config.server.ip, None);
        assert!(config.server.mdns);
        assert!(!config.features.allow_reboot);
        assert!(!config.features.auto_clean);
        assert!(!config.features.auto_remove);
        assert_eq!(config.package_manager.update_interval_mins, 360);
    }

    #[test]
    fn test_deserialization_full() {
        let yaml = r#"
server:
  port: 8080
  hostname: test-node
  ip: 10.0.0.1
  mdns: false
tls:
  ca_cert: ./ca.pem
  cert: ./cert.pem
  key: ./key.pem
features:
  allow_reboot: true
  auto_clean: true
  auto_remove: true
package_manager:
  update_interval_mins: 120
"#;
        let config: DaemonConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.server.port, Some(8080));
        assert_eq!(config.server.hostname, Some("test-node".to_string()));
        assert_eq!(
            config.server.ip,
            Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
        );
        assert!(!config.server.mdns);
        assert_eq!(config.tls.ca_cert, PathBuf::from("./ca.pem"));
        assert_eq!(config.tls.cert, PathBuf::from("./cert.pem"));
        assert_eq!(config.tls.key, PathBuf::from("./key.pem"));
        assert!(config.features.allow_reboot);
        assert!(config.features.auto_clean);
        assert!(config.features.auto_remove);
        assert_eq!(config.package_manager.update_interval_mins, 120);
    }

    #[test]
    fn test_missing_tls_section_fails() {
        let yaml = r#"
server:
  port: 8080
"#;
        let res: Result<DaemonConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err());
    }

    #[test]
    fn test_missing_tls_cert_field_fails() {
        let yaml = r#"
tls:
  ca_cert: ./ca.pem
  key: ./key.pem
"#;
        let res: Result<DaemonConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err());
    }

    #[test]
    fn test_invalid_yaml_syntax() {
        let yaml = r#"
server:
  port: [not_a_valid_port
"#;
        let res: Result<DaemonConfig, _> = serde_yaml_ng::from_str(yaml);
        assert!(res.is_err());
    }

    #[test]
    fn test_default_config_template_parsing() {
        let template = default_config_template();
        let config: DaemonConfig = serde_yaml_ng::from_str(template).unwrap();
        assert_eq!(
            config.tls.ca_cert,
            PathBuf::from("/etc/brewmble/tls/ca.pem")
        );
        assert_eq!(
            config.tls.cert,
            PathBuf::from("/etc/brewmble/tls/daemon.pem")
        );
        assert_eq!(
            config.tls.key,
            PathBuf::from("/etc/brewmble/tls/daemon-key.pem")
        );
        assert_eq!(config.server.port, None);
        assert!(config.server.mdns);
        assert_eq!(config.package_manager.update_interval_mins, 360);
    }

    #[test]
    fn test_resolve_config_path_explicit_exists() {
        let temp_dir = std::env::temp_dir().join(format!("brewmble_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("custom.yaml");
        fs::write(&config_file, "tls:\n  ca_cert: a\n  cert: b\n  key: c\n").unwrap();

        let resolved = resolve_config_path(Some(&config_file)).unwrap();
        assert_eq!(resolved, config_file);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_resolve_config_path_explicit_not_found() {
        let non_existent = PathBuf::from("/path/that/does/not/exist/brewmbled.yaml");
        let res = resolve_config_path(Some(&non_existent));
        assert!(matches!(res, Err(ConfigError::ExplicitFileNotFound(_))));
    }

    #[test]
    fn test_load_config_file() {
        let temp_dir = std::env::temp_dir().join(format!("brewmble_load_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("brewmbled.yaml");
        fs::write(
            &config_file,
            "tls:\n  ca_cert: /ca.pem\n  cert: /cert.pem\n  key: /key.pem\n",
        )
        .unwrap();

        let config = load_config(&config_file).unwrap();
        assert_eq!(config.tls.ca_cert, PathBuf::from("/ca.pem"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_save_and_init_config_file() {
        let temp_dir = std::env::temp_dir().join(format!("brewmble_init_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);
        let config_file = temp_dir.join("subdir").join("brewmbled.yaml");

        // Init creates file and parent dirs
        init_config_file(&config_file, false).unwrap();
        assert!(config_file.exists());

        // Init fails without force if exists
        let err = init_config_file(&config_file, false);
        assert!(err.is_err());

        // Init succeeds with force
        init_config_file(&config_file, true).unwrap();

        let loaded = load_config(&config_file).unwrap();
        assert_eq!(loaded.package_manager.update_interval_mins, 360);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_default_bind_address() {
        let yaml = r#"
tls:
  ca_cert: ./ca.pem
  cert: ./cert.pem
  key: ./key.pem
"#;
        let config: DaemonConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(config.server.bind_address, None);
    }

    #[test]
    fn test_explicit_loopback_bind_address() {
        let yaml = r#"
server:
  bind_address: "127.0.0.1"
tls:
  ca_cert: ./ca.pem
  cert: ./cert.pem
  key: ./key.pem
"#;
        let config: DaemonConfig = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(
            config.server.bind_address,
            Some("127.0.0.1".parse().unwrap())
        );
    }

    #[test]
    fn test_validation_valid_certs() {
        ensure_crypto_provider();
        let temp_dir = std::env::temp_dir().join(format!("brewmble_cert_{}", std::process::id()));
        let _ = fs::create_dir_all(&temp_dir);

        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = rcgen::CertificateParams::default();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut ca_params_issuer = rcgen::CertificateParams::default();
        ca_params_issuer.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let issuer = rcgen::Issuer::new(ca_params_issuer, ca_key);

        let daemon_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut daemon_params = rcgen::CertificateParams::default();
        daemon_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let daemon_cert = daemon_params.signed_by(&daemon_key, &issuer).unwrap();

        let ca_path = temp_dir.join("ca.pem");
        let cert_path = temp_dir.join("daemon.pem");
        let key_path = temp_dir.join("daemon-key.pem");

        fs::write(&ca_path, ca_cert.pem()).unwrap();
        fs::write(&cert_path, daemon_cert.pem()).unwrap();
        fs::write(&key_path, daemon_key.serialize_pem()).unwrap();

        let config = DaemonConfig {
            server: ServerConfig {
                port: Some(4712),
                hostname: Some("node-1".to_string()),
                ip: None,
                bind_address: None,
                mdns: true,
            },
            tls: TlsConfig {
                ca_cert: ca_path,
                cert: cert_path,
                key: key_path,
            },
            features: FeaturesConfig::default(),
            package_manager: PackageManagerConfig::default(),
        };

        let report = validate_config(&config);
        assert!(report.is_valid(), "Report errors: {:?}", report.errors);
        assert!(ensure_valid_config(&config).is_ok());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    fn make_test_config_with_certs(
        ca_cert: rcgen::Certificate,
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
    ) -> (DaemonConfig, tempfile::TempDir) {
        let temp_dir = tempfile::tempdir().unwrap();
        let ca_path = temp_dir.path().join("ca.pem");
        let cert_path = temp_dir.path().join("daemon.pem");
        let key_path = temp_dir.path().join("daemon-key.pem");

        fs::write(&ca_path, ca_cert.pem()).unwrap();
        fs::write(&cert_path, cert.pem()).unwrap();
        fs::write(&key_path, key.serialize_pem()).unwrap();

        let config = DaemonConfig {
            server: ServerConfig {
                port: Some(4712),
                hostname: Some("node-1".to_string()),
                ip: None,
                bind_address: None,
                mdns: true,
            },
            tls: TlsConfig {
                ca_cert: ca_path,
                cert: cert_path,
                key: key_path,
            },
            features: FeaturesConfig::default(),
            package_manager: PackageManagerConfig::default(),
        };
        (config, temp_dir)
    }

    #[test]
    fn test_validation_expired_cert() {
        ensure_crypto_provider();
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = rcgen::CertificateParams::default();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_params_for_issuer = ca_params.clone();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params_for_issuer, ca_key);

        let daemon_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut daemon_params = rcgen::CertificateParams::default();
        daemon_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        daemon_params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(400);
        daemon_params.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        let daemon_cert = daemon_params.signed_by(&daemon_key, &issuer).unwrap();

        let (config, _temp_dir) = make_test_config_with_certs(ca_cert, daemon_cert, daemon_key);
        let report = validate_config(&config);
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|e| e.contains("expired")));
    }

    #[test]
    fn test_validation_wrong_eku() {
        ensure_crypto_provider();
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = rcgen::CertificateParams::default();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_params_for_issuer = ca_params.clone();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params_for_issuer, ca_key);

        let daemon_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut daemon_params = rcgen::CertificateParams::default();
        daemon_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
        let daemon_cert = daemon_params.signed_by(&daemon_key, &issuer).unwrap();

        let (config, _temp_dir) = make_test_config_with_certs(ca_cert, daemon_cert, daemon_key);
        let report = validate_config(&config);
        assert!(!report.is_valid());
        assert!(report.errors.iter().any(|e| e.contains("ServerAuth")));
    }

    #[test]
    fn test_validation_mismatched_key() {
        ensure_crypto_provider();
        let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = rcgen::CertificateParams::default();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_params_for_issuer = ca_params.clone();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = rcgen::Issuer::new(ca_params_for_issuer, ca_key);

        let daemon_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        let mut daemon_params = rcgen::CertificateParams::default();
        daemon_params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let daemon_cert = daemon_params.signed_by(&daemon_key, &issuer).unwrap();

        let other_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();

        let (config, _temp_dir) = make_test_config_with_certs(ca_cert, daemon_cert, other_key);
        let report = validate_config(&config);
        assert!(!report.is_valid());
    }
}
