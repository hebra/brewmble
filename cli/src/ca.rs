use brewmble_rest::ClientRole;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    PKCS_ECDSA_P256_SHA256,
};
use sha2::{Digest, Sha256};
use std::error::Error;
use time::{Duration, OffsetDateTime};

const KEYRING_SERVICE: &str = "brewmble-cli";
pub(crate) const ACCOUNT_CA_CERT: &str = "cluster-ca-cert";
pub(crate) const ACCOUNT_CA_KEY: &str = "cluster-ca-key";
pub(crate) const ACCOUNT_CLIENT_CERT: &str = "client-cert";
pub(crate) const ACCOUNT_CLIENT_KEY: &str = "client-key";

const CA_CN: &str = "brewmble-cluster-ca";
pub const CA_VALIDITY_DAYS: i64 = 365 * 10;
pub const LEAF_VALIDITY_DAYS: i64 = 397;

pub enum LeafKind {
    Daemon,
    Client(ClientRole),
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>>;
    fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>>;
    fn delete(&self, account: &str) -> Result<(), Box<dyn Error>>;
}

pub trait LegacyStore: Send + Sync {
    fn read(&self, account: &str) -> Result<Option<String>, Box<dyn Error>>;
    fn delete(&self, account: &str) -> Result<(), Box<dyn Error>>;
}

pub struct PlatformLegacyStore;

impl LegacyStore for PlatformLegacyStore {
    fn read(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
        read_legacy(account)
    }
    fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
        delete_legacy(account)
    }
}

#[cfg(target_os = "macos")]
fn read_legacy(account: &str) -> Result<Option<String>, Box<dyn Error>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", KEYRING_SERVICE, "-a", account, "-w"])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let raw = String::from_utf8(output.stdout)?;
    let trimmed = raw.trim();
    if is_hex_encoding(trimmed) {
        let bytes: Result<Vec<u8>, _> = (0..trimmed.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&trimmed[i..i + 2], 16))
            .collect();
        if let Ok(bytes) = bytes
            && let Ok(text) = String::from_utf8(bytes)
        {
            return Ok(Some(text));
        }
    }
    Ok(Some(trimmed.to_string()))
}

#[cfg(target_os = "macos")]
fn is_hex_encoding(s: &str) -> bool {
    !s.is_empty() && s.len().is_multiple_of(2) && s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(target_os = "macos")]
fn delete_legacy(account: &str) -> Result<(), Box<dyn Error>> {
    std::process::Command::new("security")
        .args(["delete-generic-password", "-s", KEYRING_SERVICE, "-a", account])
        .output()?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn read_legacy(_account: &str) -> Result<Option<String>, Box<dyn Error>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn delete_legacy(_account: &str) -> Result<(), Box<dyn Error>> {
    Ok(())
}

pub(crate) const CREDENTIAL_ACCOUNTS: [&str; 4] = [
    ACCOUNT_CA_CERT,
    ACCOUNT_CA_KEY,
    ACCOUNT_CLIENT_CERT,
    ACCOUNT_CLIENT_KEY,
];

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationStatus {
    AlreadyPresent,
    Migrated,
    NotFound,
    Conflict,
    Failed(String),
}

impl std::fmt::Display for MigrationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationStatus::AlreadyPresent => write!(f, "already present"),
            MigrationStatus::Migrated => write!(f, "migrated"),
            MigrationStatus::NotFound => write!(f, "not found in legacy store"),
            MigrationStatus::Conflict => write!(f, "conflict (different value in new store)"),
            MigrationStatus::Failed(e) => write!(f, "failed: {e}"),
        }
    }
}

#[derive(Debug)]
pub struct MigrationEntry {
    pub account: &'static str,
    pub status: MigrationStatus,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub entries: Vec<MigrationEntry>,
}

impl MigrationReport {
    pub fn ca_ready(&self) -> bool {
        self.entries
            .iter()
            .filter(|e| e.account == ACCOUNT_CA_CERT || e.account == ACCOUNT_CA_KEY)
            .all(|e| {
                matches!(
                    e.status,
                    MigrationStatus::AlreadyPresent | MigrationStatus::Migrated
                )
            })
    }
}

pub fn migrate_ca(dry_run: bool) -> Result<MigrationReport, Box<dyn Error>> {
    migrate_ca_with_stores(&KeyringStore, &PlatformLegacyStore, dry_run)
}

pub fn migrate_ca_with_stores(
    store: &dyn CredentialStore,
    legacy: &dyn LegacyStore,
    dry_run: bool,
) -> Result<MigrationReport, Box<dyn Error>> {
    let mut entries = Vec::new();
    for account in CREDENTIAL_ACCOUNTS {
        let status = migrate_account(store, legacy, account, dry_run);
        entries.push(MigrationEntry { account, status });
    }
    Ok(MigrationReport { entries })
}

fn migrate_account(
    store: &dyn CredentialStore,
    legacy: &dyn LegacyStore,
    account: &'static str,
    dry_run: bool,
) -> MigrationStatus {
    let current = match store.get(account) {
        Ok(v) => v,
        Err(e) => return MigrationStatus::Failed(e.to_string()),
    };
    let legacy_value = match legacy.read(account) {
        Ok(v) => v,
        Err(e) => return MigrationStatus::Failed(e.to_string()),
    };
    match (current, legacy_value) {
        (Some(cur), Some(leg)) if cur != leg => MigrationStatus::Conflict,
        (Some(_), _) => MigrationStatus::AlreadyPresent,
        (None, None) => MigrationStatus::NotFound,
        (None, Some(value)) => {
            if dry_run {
                return MigrationStatus::Migrated;
            }
            if let Err(e) = legacy.delete(account) {
                return MigrationStatus::Failed(format!("legacy delete: {e}"));
            }
            match store.set(account, &value) {
                Ok(()) => MigrationStatus::Migrated,
                Err(e) => MigrationStatus::Failed(e.to_string()),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub struct KeyringStore;

#[cfg(not(target_os = "macos"))]
impl CredentialStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(Box::new(err)),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
        entry.set_password(value).map_err(|err| Box::new(err) as _)
    }

    fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
        delete_credential(account)
    }
}

#[cfg(target_os = "macos")]
pub struct KeyringStore;

#[cfg(target_os = "macos")]
impl CredentialStore for KeyringStore {
    fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
        macos_sec_item_get(account)
    }

    fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>> {
        macos_sec_item_set(account, value)
    }

    fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
        delete_credential(account)
    }
}

#[cfg(target_os = "macos")]
fn macos_sec_item_get(account: &str) -> Result<Option<String>, Box<dyn Error>> {
    use security_framework::item::{ItemClass, ItemSearchOptions, Limit, SearchResult};

    let mut opts = ItemSearchOptions::new();
    opts.class(ItemClass::generic_password());
    opts.service(KEYRING_SERVICE);
    opts.account(account);
    opts.load_data(true);
    opts.limit(Limit::Max(1));

    let results = match opts.search() {
        Ok(r) => r,
        Err(e) => {
            if e.code() == -25300 {
                return Ok(None);
            }
            return Err(Box::new(e));
        }
    };
    match results.into_iter().next() {
        Some(SearchResult::Data(bytes)) => Ok(Some(String::from_utf8(bytes)?)),
        Some(_) => Ok(None),
        None => Ok(None),
    }
}

#[cfg(target_os = "macos")]
fn macos_sec_item_set(account: &str, value: &str) -> Result<(), Box<dyn Error>> {
    use core_foundation::data::CFData;
    use security_framework::item::{
        ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, ItemUpdateOptions,
        ItemUpdateValue, Limit, update_item,
    };

    let data = CFData::from_buffer(value.as_bytes());

    // Try to update an existing item first.
    let mut search = ItemSearchOptions::new();
    search.class(ItemClass::generic_password());
    search.service(KEYRING_SERVICE);
    search.account(account);
    search.limit(Limit::Max(1));

    let mut update = ItemUpdateOptions::new();
    update.value = Some(ItemUpdateValue::Data(data.clone()));

    match update_item(&search, &update) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == -25300 => {
            // Item doesn't exist yet; add it.
            let mut add = ItemAddOptions::new(ItemAddValue::Data {
                class: ItemClass::generic_password(),
                data,
            });
            add.set_service(KEYRING_SERVICE);
            add.set_account_name(account);
            add.add().map_err(|e| -> Box<dyn Error> { Box::new(e) })
        }
        Err(e) => Err(Box::new(e)),
    }
}

pub fn ensure_ca() -> Result<(String, String), Box<dyn Error>> {
    ensure_ca_with_store(&KeyringStore)
}

pub fn ensure_ca_with_store(
    store: &dyn CredentialStore,
) -> Result<(String, String), Box<dyn Error>> {
    if let (Some(cert), Some(key)) = (store.get(ACCOUNT_CA_CERT)?, store.get(ACCOUNT_CA_KEY)?) {
        return Ok((cert, key));
    }

    let (cert, key) = generate_ca()?;
    store.set(ACCOUNT_CA_CERT, &cert)?;
    store.set(ACCOUNT_CA_KEY, &key)?;
    Ok((cert, key))
}

pub fn issue_leaf(cn: &str, kind: LeafKind) -> Result<(String, String), Box<dyn Error>> {
    issue_leaf_with_store(cn, kind, &KeyringStore)
}

pub fn issue_leaf_with_store(
    cn: &str,
    kind: LeafKind,
    store: &dyn CredentialStore,
) -> Result<(String, String), Box<dyn Error>> {
    let (cert, key) = issue_unstored_leaf_with_store(cn, &kind, store)?;

    if matches!(kind, LeafKind::Client(_)) {
        store.set(ACCOUNT_CLIENT_CERT, &cert)?;
        store.set(ACCOUNT_CLIENT_KEY, &key)?;
    }

    Ok((cert, key))
}

/// Issues a leaf certificate without storing it as this CLI's client certificate.
pub fn issue_unstored_leaf(cn: &str, kind: LeafKind) -> Result<(String, String), Box<dyn Error>> {
    issue_unstored_leaf_with_store(cn, &kind, &KeyringStore)
}

pub fn issue_unstored_leaf_with_store(
    cn: &str,
    kind: &LeafKind,
    store: &dyn CredentialStore,
) -> Result<(String, String), Box<dyn Error>> {
    let (ca_cert_pem, ca_key_pem) = ensure_ca_with_store(store)?;
    generate_leaf(cn, kind, &ca_cert_pem, &ca_key_pem)
}

pub fn ca_root_pem() -> Result<String, Box<dyn Error>> {
    KeyringStore
        .get(ACCOUNT_CA_CERT)?
        .ok_or_else(|| "no cluster CA found".into())
}

pub fn client_cert_pem() -> Result<String, Box<dyn Error>> {
    KeyringStore
        .get(ACCOUNT_CLIENT_CERT)?
        .ok_or_else(|| "no client certificate found".into())
}

pub fn client_key_pem() -> Result<String, Box<dyn Error>> {
    KeyringStore
        .get(ACCOUNT_CLIENT_KEY)?
        .ok_or_else(|| "no client key found".into())
}

pub fn delete_credential(account: &str) -> Result<(), Box<dyn Error>> {
    macos_sec_item_delete(account)
}

#[cfg(target_os = "macos")]
fn macos_sec_item_delete(account: &str) -> Result<(), Box<dyn Error>> {
    use security_framework::item::{ItemClass, ItemSearchOptions};

    let mut search = ItemSearchOptions::new();
    search.class(ItemClass::generic_password());
    search.service(KEYRING_SERVICE);
    search.account(account);
    search
        .delete()
        .map_err(|e| -> Box<dyn Error> { Box::new(e) })
}

#[cfg(not(target_os = "macos"))]
fn macos_sec_item_delete(account: &str) -> Result<(), Box<dyn Error>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, account)?;
    entry
        .delete_credential()
        .map_err(|e| -> Box<dyn Error> { Box::new(e) })
}

pub fn fingerprint(cert_pem: &str) -> Result<String, Box<dyn Error>> {
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    let cert = certs
        .into_iter()
        .next()
        .ok_or("no certificate found in PEM")?;
    let hash = Sha256::digest(cert.as_ref());
    Ok(hash
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":"))
}

fn generate_ca() -> Result<(String, String), Box<dyn Error>> {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name.push(DnType::CommonName, CA_CN);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    let not_before = OffsetDateTime::now_utc() - Duration::days(1);
    params.not_before = not_before;
    params.not_after = not_before + Duration::days(CA_VALIDITY_DAYS);

    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn generate_leaf(
    cn: &str,
    kind: &LeafKind,
    _ca_cert_pem: &str,
    ca_key_pem: &str,
) -> Result<(String, String), Box<dyn Error>> {
    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.distinguished_name.push(DnType::CommonName, CA_CN);
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.subject_alt_names = vec![rcgen::SanType::DnsName(cn.parse()?)];

    match kind {
        LeafKind::Daemon => {
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        }
        LeafKind::Client(role) => {
            params
                .distinguished_name
                .push(DnType::OrganizationalUnitName, role.subject_ou());
            params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        }
    }

    let not_before = OffsetDateTime::now_utc() - Duration::days(1);
    params.not_before = not_before;
    params.not_after = not_before + Duration::days(LEAF_VALIDITY_DAYS);

    let cert = params.signed_by(&leaf_key, &issuer)?;
    Ok((cert.pem(), leaf_key.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
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

    #[test]
    fn test_ensure_ca_creates_and_round_trips() {
        let store = MemoryStore::default();
        let (cert1, key1) = ensure_ca_with_store(&store).unwrap();
        assert!(!cert1.is_empty());
        assert!(!key1.is_empty());

        let (cert2, key2) = ensure_ca_with_store(&store).unwrap();
        assert_eq!(cert1, cert2);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_issue_leaf_daemon_has_server_auth() {
        let store = MemoryStore::default();
        let (_ca_cert, _ca_key) = ensure_ca_with_store(&store).unwrap();
        let (cert_pem, key_pem) =
            issue_leaf_with_store("daemon1", LeafKind::Daemon, &store).unwrap();
        assert!(cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(key_pem.contains("BEGIN PRIVATE KEY"));
    }

    #[test]
    fn test_issue_leaf_client_stores_in_keyring() {
        let store = MemoryStore::default();
        let (_ca_cert, _ca_key) = ensure_ca_with_store(&store).unwrap();
        let (_cert_pem, _key_pem) =
            issue_leaf_with_store("client1", LeafKind::Client(ClientRole::Operator), &store)
                .unwrap();
        assert!(store.get(ACCOUNT_CLIENT_CERT).unwrap().is_some());
        assert!(store.get(ACCOUNT_CLIENT_KEY).unwrap().is_some());
    }

    fn subject_role_ous(cert_pem: &str) -> Vec<String> {
        let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes()).unwrap();
        let cert = pem.parse_x509().unwrap();
        cert.subject()
            .iter_organizational_unit()
            .map(|ou| ou.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn test_client_leaf_carries_role() {
        let store = MemoryStore::default();
        for role in [ClientRole::Read, ClientRole::Operator] {
            let (cert_pem, _key) =
                issue_unstored_leaf_with_store("client1", &LeafKind::Client(role), &store).unwrap();
            assert_eq!(subject_role_ous(&cert_pem), vec![role.subject_ou()]);
        }
    }

    #[test]
    fn test_daemon_leaf_has_no_role() {
        let store = MemoryStore::default();
        let (cert_pem, _key) =
            issue_unstored_leaf_with_store("daemon1", &LeafKind::Daemon, &store).unwrap();
        assert!(subject_role_ous(&cert_pem).is_empty());
    }

    #[test]
    fn test_unstored_client_leaf_does_not_replace_keyring_client() {
        let store = MemoryStore::default();
        let (stored, _) =
            issue_leaf_with_store("cli", LeafKind::Client(ClientRole::Operator), &store).unwrap();
        let (other, _) =
            issue_unstored_leaf_with_store("monitor", &LeafKind::Client(ClientRole::Read), &store)
                .unwrap();
        assert_ne!(stored, other);
        assert_eq!(store.get(ACCOUNT_CLIENT_CERT).unwrap(), Some(stored));
    }

    #[test]
    fn test_fingerprint() {
        let store = MemoryStore::default();
        let (cert, _key) = ensure_ca_with_store(&store).unwrap();
        let fp = fingerprint(&cert).unwrap();
        assert_eq!(fp.len(), 95); // 32 bytes -> 64 hex chars + 31 colons
        assert!(fp.contains(':'));
    }

    #[derive(Default, Clone)]
    struct MemoryLegacyStore {
        data: Arc<Mutex<HashMap<String, String>>>,
    }

    impl LegacyStore for MemoryLegacyStore {
        fn read(&self, account: &str) -> Result<Option<String>, Box<dyn Error>> {
            Ok(self.data.lock().unwrap().get(account).cloned())
        }
        fn delete(&self, account: &str) -> Result<(), Box<dyn Error>> {
            self.data.lock().unwrap().remove(account);
            Ok(())
        }
    }

    impl MemoryLegacyStore {
        fn insert(&self, account: &str, value: &str) {
            self.data
                .lock()
                .unwrap()
                .insert(account.to_string(), value.to_string());
        }
    }

    #[test]
    fn test_migrate_all_from_legacy() {
        let store = MemoryStore::default();
        let legacy = MemoryLegacyStore::default();
        for account in CREDENTIAL_ACCOUNTS {
            legacy.insert(account, &format!("value-{account}"));
        }
        let report = migrate_ca_with_stores(&store, &legacy, false).unwrap();
        assert!(report.ca_ready());
        for entry in &report.entries {
            assert_eq!(entry.status, MigrationStatus::Migrated);
        }
        for account in CREDENTIAL_ACCOUNTS {
            assert_eq!(
                store.get(account).unwrap(),
                Some(format!("value-{account}"))
            );
        }
    }

    #[test]
    fn test_migrate_already_present() {
        let store = MemoryStore::default();
        let legacy = MemoryLegacyStore::default();
        store.set(ACCOUNT_CA_CERT, "new-value").unwrap();
        legacy.insert(ACCOUNT_CA_CERT, "new-value");
        let report = migrate_ca_with_stores(&store, &legacy, false).unwrap();
        let entry = report
            .entries
            .iter()
            .find(|e| e.account == ACCOUNT_CA_CERT)
            .unwrap();
        assert_eq!(entry.status, MigrationStatus::AlreadyPresent);
    }

    #[test]
    fn test_migrate_conflict() {
        let store = MemoryStore::default();
        let legacy = MemoryLegacyStore::default();
        store.set(ACCOUNT_CA_CERT, "new-value").unwrap();
        legacy.insert(ACCOUNT_CA_CERT, "old-value");
        let report = migrate_ca_with_stores(&store, &legacy, false).unwrap();
        let entry = report
            .entries
            .iter()
            .find(|e| e.account == ACCOUNT_CA_CERT)
            .unwrap();
        assert_eq!(entry.status, MigrationStatus::Conflict);
        assert_eq!(store.get(ACCOUNT_CA_CERT).unwrap().unwrap(), "new-value");
    }

    #[test]
    fn test_migrate_not_found() {
        let store = MemoryStore::default();
        let legacy = MemoryLegacyStore::default();
        let report = migrate_ca_with_stores(&store, &legacy, false).unwrap();
        assert!(!report.ca_ready());
        for entry in &report.entries {
            assert_eq!(entry.status, MigrationStatus::NotFound);
        }
    }

    #[test]
    fn test_migrate_dry_run_writes_nothing() {
        let store = MemoryStore::default();
        let legacy = MemoryLegacyStore::default();
        legacy.insert(ACCOUNT_CA_CERT, "value");
        let report = migrate_ca_with_stores(&store, &legacy, true).unwrap();
        let entry = report
            .entries
            .iter()
            .find(|e| e.account == ACCOUNT_CA_CERT)
            .unwrap();
        assert_eq!(entry.status, MigrationStatus::Migrated);
        assert!(store.get(ACCOUNT_CA_CERT).unwrap().is_none());
    }
}
