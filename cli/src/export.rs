use crate::ca::{
    self, ACCOUNT_CA_CERT, ACCOUNT_CA_KEY, ACCOUNT_CLIENT_CERT, ACCOUNT_CLIENT_KEY,
    CREDENTIAL_ACCOUNTS, CredentialStore,
};
use crate::{Config, sanitize::sanitize_display};
use age::secrecy::{ExposeSecret, SecretString};
use rustls::pki_types::CertificateDer;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

const EXPORT_FORMAT: &str = "brewmble-export";
const EXPORT_VERSION: u32 = 1;

type ExistingCredential = (&'static str, Option<String>);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportBundle {
    format: String,
    version: u32,
    created_at: String,
    config: Config,
    credentials: ExportCredentials,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportCredentials {
    cluster_ca_cert: String,
    cluster_ca_key: String,
    client_cert: String,
    client_key: String,
}

impl ExportCredentials {
    fn entries(&self) -> [(&'static str, &str); 4] {
        [
            (ACCOUNT_CA_CERT, &self.cluster_ca_cert),
            (ACCOUNT_CA_KEY, &self.cluster_ca_key),
            (ACCOUNT_CLIENT_CERT, &self.client_cert),
            (ACCOUNT_CLIENT_KEY, &self.client_key),
        ]
    }
}

impl ExportBundle {
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        if self.format != EXPORT_FORMAT {
            return Err(format!("unsupported export format '{}'", self.format).into());
        }
        if self.version != EXPORT_VERSION {
            return Err(format!("unsupported export version {}", self.version).into());
        }
        if !self
            .config
            .profiles
            .contains_key(&self.config.active_profile)
        {
            return Err(format!(
                "active profile '{}' is missing from the export",
                self.config.active_profile
            )
            .into());
        }
        if !self.config.nodes.is_empty() {
            return Err("export contains unsupported legacy top-level nodes".into());
        }
        self.credentials.validate()
    }
}

impl ExportCredentials {
    fn validate(&self) -> Result<(), Box<dyn Error>> {
        let ca_der =
            validate_certificate_pair("cluster CA", &self.cluster_ca_cert, &self.cluster_ca_key)?;
        let client_der = validate_certificate_pair("client", &self.client_cert, &self.client_key)?;
        let ca_cert = parse_x509(ca_der.as_ref(), "cluster CA")?;
        let client_cert = parse_x509(client_der.as_ref(), "client")?;

        if !ca_cert.is_ca() {
            return Err("cluster CA certificate is not a CA".into());
        }
        if !ca_cert.validity().is_valid() {
            return Err("cluster CA certificate is expired or not yet valid".into());
        }
        if !client_cert.validity().is_valid() {
            return Err("client certificate is expired or not yet valid".into());
        }
        ca_cert
            .verify_signature(None)
            .map_err(|_| "cluster CA certificate is not correctly self-signed")?;
        client_cert
            .verify_signature(Some(ca_cert.public_key()))
            .map_err(|_| "client certificate was not signed by the exported cluster CA")?;
        let eku = client_cert
            .extended_key_usage()
            .map_err(|_| "client certificate has invalid extended key usage")?
            .ok_or("client certificate is missing extended key usage")?;
        if !eku.value.client_auth {
            return Err("client certificate does not allow client authentication".into());
        }
        Ok(())
    }
}

fn validate_certificate_pair(
    label: &str,
    cert_pem: &str,
    key_pem: &str,
) -> Result<CertificateDer<'static>, Box<dyn Error>> {
    let mut reader = io::BufReader::new(cert_pem.as_bytes());
    let mut certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| format!("{label} certificate is not valid PEM"))?;
    if certs.len() != 1 {
        return Err(format!("{label} certificate PEM must contain exactly one certificate").into());
    }
    let cert = certs.remove(0);
    let mut key_reader = io::BufReader::new(key_pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|_| format!("{label} private key is not valid PEM"))?
        .ok_or_else(|| format!("{label} private key is missing"))?;
    rustls::sign::CertifiedKey::from_der(
        vec![cert.clone()],
        key,
        &rustls::crypto::ring::default_provider(),
    )
    .map_err(|_| format!("{label} certificate and private key do not match"))?;
    Ok(cert)
}

fn parse_x509<'a>(der: &'a [u8], label: &str) -> Result<X509Certificate<'a>, Box<dyn Error>> {
    let (remaining, cert) = X509Certificate::from_der(der)
        .map_err(|_| format!("{label} certificate is not valid X.509"))?;
    if !remaining.is_empty() {
        return Err(format!("{label} certificate contains trailing data").into());
    }
    Ok(cert)
}

fn required_credential(
    store: &dyn CredentialStore,
    account: &str,
) -> Result<String, Box<dyn Error>> {
    store.get(account)?.ok_or_else(|| {
        format!(
            "required credential '{account}' was not found; run `brewmble ca init` and `brewmble cert issue --kind client`"
        )
        .into()
    })
}

fn build_bundle(
    config: &Config,
    store: &dyn CredentialStore,
) -> Result<ExportBundle, Box<dyn Error>> {
    Ok(ExportBundle {
        format: EXPORT_FORMAT.to_string(),
        version: EXPORT_VERSION,
        created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
        config: config.clone(),
        credentials: ExportCredentials {
            cluster_ca_cert: required_credential(store, ACCOUNT_CA_CERT)?,
            cluster_ca_key: required_credential(store, ACCOUNT_CA_KEY)?,
            client_cert: required_credential(store, ACCOUNT_CLIENT_CERT)?,
            client_key: required_credential(store, ACCOUNT_CLIENT_KEY)?,
        },
    })
}

fn encrypt_bundle(
    bundle: &ExportBundle,
    passphrase: SecretString,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let plaintext = serde_json::to_vec(bundle)?;
    let recipient = age::scrypt::Recipient::new(passphrase);
    Ok(age::encrypt(&recipient, &plaintext)?)
}

fn decrypt_bundle(
    encrypted: &[u8],
    passphrase: SecretString,
) -> Result<ExportBundle, Box<dyn Error>> {
    let identity = age::scrypt::Identity::new(passphrase);
    let plaintext = age::decrypt(&identity, encrypted)
        .map_err(|_| "failed to decrypt export: incorrect passphrase or invalid age file")?;
    let bundle: ExportBundle = serde_json::from_slice(&plaintext)
        .map_err(|_| "decrypted export has an invalid payload")?;
    bundle.validate()?;
    Ok(bundle)
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || match (fs::canonicalize(left), fs::canonicalize(right)) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn write_export_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    let result = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn print_summary(bundle: &ExportBundle) {
    let mut profiles = bundle.config.profiles.keys().cloned().collect::<Vec<_>>();
    profiles.sort();
    let profiles = profiles
        .iter()
        .map(|name| sanitize_display(name))
        .collect::<Vec<_>>()
        .join(", ");
    let nodes = bundle
        .config
        .profiles
        .values()
        .map(|profile| profile.nodes.len())
        .sum::<usize>();
    println!("Export version: {}", bundle.version);
    println!("Profiles: {profiles}");
    println!("Nodes: {nodes}");
    match ca::fingerprint(&bundle.credentials.cluster_ca_cert) {
        Ok(fingerprint) => println!("Cluster CA fingerprint: {fingerprint}"),
        Err(_) => println!("Cluster CA fingerprint: unavailable"),
    }
    match ca::fingerprint(&bundle.credentials.client_cert) {
        Ok(fingerprint) => println!("Client certificate fingerprint: {fingerprint}"),
        Err(_) => println!("Client certificate fingerprint: unavailable"),
    }
}

pub(crate) fn run_export(
    config: &Config,
    store: &dyn CredentialStore,
    output: &Path,
    force: bool,
    config_path: &Path,
) -> Result<(), Box<dyn Error>> {
    if same_path(output, config_path) {
        return Err("export output must not be the CLI configuration file".into());
    }
    if output.exists() && !force {
        return Err(format!(
            "export file '{}' already exists; use --force to overwrite",
            output.display()
        )
        .into());
    }
    let bundle = build_bundle(config, store)?;
    bundle.validate()?;
    let passphrase = SecretString::from(rpassword::prompt_password("Encryption passphrase: ")?);
    if passphrase.expose_secret().is_empty() {
        return Err("passphrase must not be empty".into());
    }
    let confirmation = SecretString::from(rpassword::prompt_password("Confirm passphrase: ")?);
    if passphrase.expose_secret() != confirmation.expose_secret() {
        return Err("passphrases do not match".into());
    }
    let encrypted = encrypt_bundle(&bundle, passphrase)?;
    write_export_atomically(output, &encrypted)?;
    println!("Encrypted export written to {}", output.display());
    print_summary(&bundle);
    Ok(())
}

fn existing_credentials(
    store: &dyn CredentialStore,
) -> Result<Vec<ExistingCredential>, Box<dyn Error>> {
    CREDENTIAL_ACCOUNTS
        .iter()
        .map(|account| Ok((*account, store.get(account)?)))
        .collect()
}

fn rollback_credentials(
    store: &dyn CredentialStore,
    previous: &[(&'static str, Option<String>)],
    changed: &[&'static str],
) -> Option<String> {
    let mut failures = Vec::new();
    for account in changed.iter().rev() {
        let old_value = previous
            .iter()
            .find(|(old_account, _)| old_account == account)
            .and_then(|(_, value)| value.as_ref());
        let result = match old_value {
            Some(value) => store.set(account, value),
            None => store.delete(account),
        };
        if let Err(error) = result {
            failures.push(format!("{account}: {error}"));
        }
    }
    (!failures.is_empty()).then(|| failures.join(", "))
}

fn import_error(message: String, rollback_error: Option<String>) -> Box<dyn Error> {
    match rollback_error {
        Some(rollback) => format!("{message}; credential rollback also failed: {rollback}").into(),
        None => message.into(),
    }
}

fn import_bundle(
    bundle: &ExportBundle,
    store: &dyn CredentialStore,
    config_path: &Path,
    force: bool,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    bundle.validate()?;
    let previous = existing_credentials(store)?;
    let config_exists = config_path.exists();
    if !force && (config_exists || previous.iter().any(|(_, value)| value.is_some())) {
        return Err(
            "existing configuration or credentials found; use --force to replace them".into(),
        );
    }
    if dry_run {
        println!("Dry run — no changes written.");
        print_summary(bundle);
        return Ok(());
    }

    let serialized = serde_yaml_ng::to_string(&bundle.config)?;
    let mut changed = Vec::new();
    for (account, value) in bundle.credentials.entries() {
        let result = store.set(account, value);
        changed.push(account);
        if let Err(error) = result {
            let rollback = rollback_credentials(store, &previous, &changed);
            return Err(import_error(
                format!("failed to import credential '{account}': {error}"),
                rollback,
            ));
        }
    }
    if let Err(error) = crate::write_config_atomically(config_path, &serialized) {
        let rollback = rollback_credentials(store, &previous, &changed);
        return Err(import_error(
            format!("failed to write imported configuration: {error}"),
            rollback,
        ));
    }
    println!("Imported encrypted export into {}", config_path.display());
    print_summary(bundle);
    Ok(())
}

pub(crate) fn run_import(
    store: &dyn CredentialStore,
    input: &Path,
    config_path: &Path,
    force: bool,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    if same_path(input, config_path) {
        return Err("import input must not be the CLI configuration file".into());
    }
    let encrypted = fs::read(input)?;
    let passphrase = SecretString::from(rpassword::prompt_password("Decryption passphrase: ")?);
    if passphrase.expose_secret().is_empty() {
        return Err("passphrase must not be empty".into());
    }
    let bundle = decrypt_bundle(&encrypted, passphrase)?;
    import_bundle(&bundle, store, config_path, force, dry_run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::{LeafKind, ensure_ca_with_store, issue_leaf_with_store};
    use crate::{NodeConfig, ProfileConfig};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    struct MemoryStore {
        data: Arc<Mutex<HashMap<String, String>>>,
    }

    impl CredentialStore for MemoryStore {
        fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
            Ok(self.data.lock().unwrap().get(account).cloned())
        }

        fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>> {
            self.data
                .lock()
                .unwrap()
                .insert(account.to_string(), value.to_string());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
            self.data.lock().unwrap().remove(account);
            Ok(())
        }
    }

    struct FailingStore {
        inner: MemoryStore,
        fail_on: &'static str,
    }

    impl CredentialStore for FailingStore {
        fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
            self.inner.get(account)
        }

        fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>> {
            if account == self.fail_on {
                return Err("injected set failure".into());
            }
            self.inner.set(account, value)
        }

        fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
            self.inner.delete(account)
        }
    }

    struct TempPath(PathBuf);

    impl TempPath {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("brewmble-export-{name}-{}", std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
            let _ = fs::remove_file(self.0.with_extension("tmp"));
        }
    }

    fn populated_store() -> MemoryStore {
        let store = MemoryStore::default();
        ensure_ca_with_store(&store).unwrap();
        issue_leaf_with_store("test-client", LeafKind::Client, &store).unwrap();
        store
    }

    fn valid_bundle() -> ExportBundle {
        build_bundle(&Config::default(), &populated_store()).unwrap()
    }

    #[test]
    fn encrypted_export_round_trip_preserves_profiles_credentials_and_pins() {
        let source = populated_store();
        let mut config = Config::default();
        config.profiles.insert(
            "production".to_string(),
            ProfileConfig {
                nodes: vec![NodeConfig {
                    name: Some("node-1".to_string()),
                    address: "192.168.1.50:4712".to_string(),
                    fingerprint: Some("aa:bb".to_string()),
                }],
            },
        );
        config.active_profile = "production".to_string();
        let bundle = build_bundle(&config, &source).unwrap();
        let encrypted =
            encrypt_bundle(&bundle, SecretString::from("correct horse".to_string())).unwrap();

        assert!(!String::from_utf8_lossy(&encrypted).contains("PRIVATE KEY"));
        let decrypted =
            decrypt_bundle(&encrypted, SecretString::from("correct horse".to_string())).unwrap();
        assert_eq!(decrypted.format, EXPORT_FORMAT);
        assert_eq!(decrypted.version, EXPORT_VERSION);
        assert_eq!(decrypted.config.active_profile, "production");
        assert_eq!(decrypted.config.profiles.len(), 2);
        assert_eq!(
            decrypted.config.profiles["production"].nodes[0]
                .fingerprint
                .as_deref(),
            Some("aa:bb")
        );
        assert_eq!(
            decrypted.credentials.client_key,
            source.get(ACCOUNT_CLIENT_KEY).unwrap().unwrap()
        );
    }

    #[test]
    fn decrypt_rejects_wrong_passphrase() {
        let encrypted = encrypt_bundle(
            &valid_bundle(),
            SecretString::from("correct passphrase".to_string()),
        )
        .unwrap();
        let error = decrypt_bundle(&encrypted, SecretString::from("wrong".to_string()))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("incorrect passphrase"));
    }

    #[test]
    fn decrypt_rejects_non_age_input() {
        let error = decrypt_bundle(
            b"not an age file",
            SecretString::from("passphrase".to_string()),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("invalid age file"));
    }

    #[test]
    fn export_requires_all_credentials() {
        let error = build_bundle(&Config::default(), &MemoryStore::default())
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(ACCOUNT_CA_CERT));
    }

    #[test]
    fn validation_rejects_unknown_format_and_version() {
        let mut bundle = valid_bundle();
        bundle.format = "other".to_string();
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("format")
        );
        bundle.format = EXPORT_FORMAT.to_string();
        bundle.version = 2;
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
    }

    #[test]
    fn validation_rejects_missing_active_profile_and_legacy_nodes() {
        let mut bundle = valid_bundle();
        bundle.config.active_profile = "missing".to_string();
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("missing")
        );
        bundle.config.active_profile = "default".to_string();
        bundle.config.nodes.push(NodeConfig {
            name: None,
            address: "127.0.0.1:4712".to_string(),
            fingerprint: None,
        });
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("legacy")
        );
    }

    #[test]
    fn validation_rejects_mismatched_private_key() {
        let other = populated_store();
        let mut bundle = valid_bundle();
        bundle.credentials.client_key = other.get(ACCOUNT_CLIENT_KEY).unwrap().unwrap();
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("do not match")
        );
    }

    #[test]
    fn validation_rejects_non_ca_cluster_certificate() {
        let original = valid_bundle();
        let mut bundle = valid_bundle();
        bundle.credentials.cluster_ca_cert = original.credentials.client_cert;
        bundle.credentials.cluster_ca_key = original.credentials.client_key;
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not a CA")
        );
    }

    #[test]
    fn validation_rejects_certificate_from_another_ca() {
        let other = populated_store();
        let mut bundle = valid_bundle();
        bundle.credentials.cluster_ca_cert = other.get(ACCOUNT_CA_CERT).unwrap().unwrap();
        bundle.credentials.cluster_ca_key = other.get(ACCOUNT_CA_KEY).unwrap().unwrap();
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not signed")
        );
    }

    #[test]
    fn validation_rejects_daemon_certificate_as_client() {
        let source = populated_store();
        let (cert, key) = issue_leaf_with_store("daemon", LeafKind::Daemon, &source).unwrap();
        let mut bundle = build_bundle(&Config::default(), &source).unwrap();
        bundle.credentials.client_cert = cert;
        bundle.credentials.client_key = key;
        assert!(
            bundle
                .validate()
                .unwrap_err()
                .to_string()
                .contains("client authentication")
        );
    }

    #[test]
    fn dry_run_import_writes_no_configuration_or_credentials() {
        let destination = MemoryStore::default();
        let config_path = TempPath::new("dry-run");
        import_bundle(&valid_bundle(), &destination, &config_path.0, false, true).unwrap();
        assert!(!config_path.0.exists());
        for account in CREDENTIAL_ACCOUNTS {
            assert_eq!(destination.get(account).unwrap(), None);
        }
    }

    #[test]
    fn import_restores_configuration_and_credentials() {
        let bundle = valid_bundle();
        let destination = MemoryStore::default();
        let config_path = TempPath::new("restore");
        import_bundle(&bundle, &destination, &config_path.0, false, false).unwrap();

        let config = crate::load_config(&config_path.0).unwrap();
        assert_eq!(config.active_profile, bundle.config.active_profile);
        assert_eq!(config.profiles.len(), bundle.config.profiles.len());
        for (account, value) in bundle.credentials.entries() {
            assert_eq!(destination.get(account).unwrap().as_deref(), Some(value));
        }
    }

    #[test]
    fn import_refuses_existing_configuration_without_force() {
        let destination = MemoryStore::default();
        let config_path = TempPath::new("existing-config");
        fs::write(&config_path.0, "original: true\n").unwrap();
        let error = import_bundle(&valid_bundle(), &destination, &config_path.0, false, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("use --force"));
        assert_eq!(
            fs::read_to_string(&config_path.0).unwrap(),
            "original: true\n"
        );
    }

    #[test]
    fn import_refuses_existing_credentials_without_force() {
        let destination = populated_store();
        let config_path = TempPath::new("existing-credentials");
        let error = import_bundle(&valid_bundle(), &destination, &config_path.0, false, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("use --force"));
        assert!(!config_path.0.exists());
    }

    #[test]
    fn force_import_replaces_existing_state() {
        let bundle = valid_bundle();
        let destination = populated_store();
        let old_key = destination.get(ACCOUNT_CLIENT_KEY).unwrap().unwrap();
        let config_path = TempPath::new("force");
        fs::write(
            &config_path.0,
            "active_profile: old\nprofiles:\n  old:\n    nodes: []\n",
        )
        .unwrap();
        import_bundle(&bundle, &destination, &config_path.0, true, false).unwrap();
        assert_ne!(
            destination.get(ACCOUNT_CLIENT_KEY).unwrap().unwrap(),
            old_key
        );
        let config = crate::load_config(&config_path.0).unwrap();
        assert!(!config.profiles.contains_key("old"));
    }

    #[test]
    fn credential_failure_rolls_back_previous_credentials_and_keeps_config() {
        let bundle = valid_bundle();
        let inner = populated_store();
        let previous = existing_credentials(&inner).unwrap();
        let destination = FailingStore {
            inner,
            fail_on: ACCOUNT_CLIENT_KEY,
        };
        let config_path = TempPath::new("credential-rollback");
        fs::write(&config_path.0, "original: true\n").unwrap();
        let error = import_bundle(&bundle, &destination, &config_path.0, true, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("injected set failure"));
        assert_eq!(
            fs::read_to_string(&config_path.0).unwrap(),
            "original: true\n"
        );
        for (account, value) in previous {
            assert_eq!(destination.get(account).unwrap(), value);
        }
    }

    #[test]
    fn configuration_failure_rolls_back_new_credentials() {
        let destination = MemoryStore::default();
        let config_path = std::env::temp_dir().join(format!(
            "brewmble-export-missing-parent-{}/nested/config.yaml",
            std::process::id()
        ));
        let error = import_bundle(&valid_bundle(), &destination, &config_path, false, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("failed to write imported configuration"));
        for account in CREDENTIAL_ACCOUNTS {
            assert_eq!(destination.get(account).unwrap(), None);
        }
    }

    #[test]
    fn atomic_export_write_round_trip_uses_private_permissions() {
        let path = TempPath::new("permissions.age");
        write_export_atomically(&path.0, b"encrypted").unwrap();
        assert_eq!(fs::read(&path.0).unwrap(), b"encrypted");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path.0).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn same_path_detects_direct_and_canonical_matches() {
        let path = TempPath::new("same-path");
        fs::write(&path.0, "content").unwrap();
        assert!(same_path(&path.0, &path.0));
        assert!(same_path(&path.0, &path.0.canonicalize().unwrap()));
        assert!(!same_path(&path.0, Path::new("different.age")));
    }
}
