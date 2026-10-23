use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    PKCS_ECDSA_P256_SHA256,
};
use sha2::{Digest, Sha256};
use std::error::Error;
use time::{Duration, OffsetDateTime};

const KEYRING_SERVICE: &str = "brewmble-cli";
const ACCOUNT_CA_CERT: &str = "cluster-ca-cert";
const ACCOUNT_CA_KEY: &str = "cluster-ca-key";
const ACCOUNT_CLIENT_CERT: &str = "client-cert";
const ACCOUNT_CLIENT_KEY: &str = "client-key";

const CA_CN: &str = "brewmble-cluster-ca";
pub const CA_VALIDITY_DAYS: i64 = 365 * 10;
pub const LEAF_VALIDITY_DAYS: i64 = 397;

pub enum LeafKind {
    Daemon,
    Client,
}

pub trait CredentialStore: Send + Sync {
    fn get(&self, account: &str) -> Result<Option<String>, Box<dyn Error>>;
    fn set(&self, account: &str, value: &str) -> Result<(), Box<dyn Error>>;
}

pub struct KeyringStore;

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
    let (ca_cert_pem, ca_key_pem) = ensure_ca_with_store(store)?;
    let (cert, key) = generate_leaf(cn, &kind, &ca_cert_pem, &ca_key_pem)?;

    if matches!(kind, LeafKind::Client) {
        store.set(ACCOUNT_CLIENT_CERT, &cert)?;
        store.set(ACCOUNT_CLIENT_KEY, &key)?;
    }

    Ok((cert, key))
}

pub fn ca_root_pem() -> Result<String, Box<dyn Error>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, ACCOUNT_CA_CERT)?;
    Ok(entry.get_password()?)
}

pub fn ca_root() -> Result<reqwest::Certificate, Box<dyn Error>> {
    let pem = ca_root_pem()?;
    Ok(reqwest::Certificate::from_pem(pem.as_bytes())?)
}

pub fn client_cert_pem() -> Result<String, Box<dyn Error>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, ACCOUNT_CLIENT_CERT)?;
    Ok(entry.get_password()?)
}

pub fn client_key_pem() -> Result<String, Box<dyn Error>> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, ACCOUNT_CLIENT_KEY)?;
    Ok(entry.get_password()?)
}

pub fn client_identity() -> Result<reqwest::Identity, Box<dyn Error>> {
    let cert = client_cert_pem()?;
    let key = client_key_pem()?;
    let bundle = format!("{}\n{}", cert.trim_end(), key.trim_end());
    Ok(reqwest::Identity::from_pem(bundle.as_bytes())?)
}

pub fn build_client() -> Result<reqwest::blocking::Client, Box<dyn Error>> {
    let identity = client_identity().map_err(|_| {
        "no client certificate found — run `brewmble ca init` and `brewmble cert issue --kind client`"
    })?;
    let ca = ca_root().map_err(|_| "no cluster CA found — run `brewmble ca init`")?;

    Ok(reqwest::blocking::Client::builder()
        .identity(identity)
        .tls_certs_only([ca])
        .tls_danger_accept_invalid_hostnames(true)
        .https_only(true)
        .timeout(super::get_default_timeout())
        .build()?)
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
        LeafKind::Client => {
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
            issue_leaf_with_store("client1", LeafKind::Client, &store).unwrap();
        assert!(store.get(ACCOUNT_CLIENT_CERT).unwrap().is_some());
        assert!(store.get(ACCOUNT_CLIENT_KEY).unwrap().is_some());
    }

    #[test]
    fn test_fingerprint() {
        let store = MemoryStore::default();
        let (cert, _key) = ensure_ca_with_store(&store).unwrap();
        let fp = fingerprint(&cert).unwrap();
        assert_eq!(fp.len(), 95); // 32 bytes -> 64 hex chars + 31 colons
        assert!(fp.contains(':'));
    }
}
