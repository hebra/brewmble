//! Client-certificate authorization: a fingerprint allow-list plus a per-route role check.

use axum::{
    Extension, Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::{AddExtension, Next},
    response::{IntoResponse, Response},
};
use axum_server::accept::Accept;
use brewmble_rest::{CLIENT_ROLE_OU_PREFIX, ClientRole};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::server::TlsStream;
use tower::Layer;
use tracing::{info, warn};

use crate::config;

/// Identity of the client certificate presented on a TLS connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub fingerprint: String,
    pub role: Option<ClientRole>,
}

impl ClientIdentity {
    pub fn from_der(der: &[u8]) -> Self {
        Self {
            fingerprint: fingerprint(der),
            role: role_from_der(der),
        }
    }

    /// Role used for authorization; certificates without a valid role get `read`.
    pub fn effective_role(&self) -> ClientRole {
        self.role.unwrap_or(ClientRole::Read)
    }
}

/// SHA-256 fingerprint of a DER certificate, formatted like the CLI prints it (`ab:cd:...`).
pub fn fingerprint(der: &[u8]) -> String {
    format_hex(&Sha256::digest(der))
}

fn format_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Normalizes a configured fingerprint (with or without colons, any case) to `ab:cd:...` form.
pub fn normalize_fingerprint(value: &str) -> Option<String> {
    let hex: String = value.trim().chars().filter(|c| *c != ':').collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bytes: Vec<u8> = (0..32)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<_>>()?;
    Some(format_hex(&bytes))
}

/// Reads the role from the certificate subject's `OU=brewmble-role:<role>` attribute.
/// Returns `None` when the attribute is missing, unknown, or present more than once.
pub fn role_from_der(der: &[u8]) -> Option<ClientRole> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let mut roles = cert
        .subject()
        .iter_organizational_unit()
        .filter_map(|attr| attr.as_str().ok())
        .filter_map(|value| value.strip_prefix(CLIENT_ROLE_OU_PREFIX));
    let role = ClientRole::parse(roles.next()?)?;
    if roles.next().is_some() {
        return None;
    }
    Some(role)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denial {
    NotAllowed,
    InsufficientRole { have: ClientRole, need: ClientRole },
}

impl Denial {
    fn message(&self) -> String {
        match self {
            Denial::NotAllowed => "client certificate is not allowed on this daemon".to_string(),
            Denial::InsufficientRole { have, need } => {
                format!(
                    "client certificate role '{have}' does not permit this request (requires '{need}')"
                )
            }
        }
    }
}

/// Live client allow-list. `None` accepts every certificate signed by the cluster CA.
#[derive(Debug, Clone, Default)]
pub struct ClientAccess {
    allowed: Arc<RwLock<Option<HashSet<String>>>>,
}

impl ClientAccess {
    pub fn new(allowed: Option<HashSet<String>>) -> Self {
        Self {
            allowed: Arc::new(RwLock::new(allowed)),
        }
    }

    pub fn replace(&self, allowed: Option<HashSet<String>>) {
        *self.allowed.write().unwrap_or_else(|e| e.into_inner()) = allowed;
    }

    pub fn check(&self, identity: &ClientIdentity, required: ClientRole) -> Result<(), Denial> {
        let allowed = self.allowed.read().unwrap_or_else(|e| e.into_inner());
        if let Some(allowed) = allowed.as_ref()
            && !allowed.contains(&identity.fingerprint)
        {
            return Err(Denial::NotAllowed);
        }
        let have = identity.effective_role();
        if !have.allows(required) {
            return Err(Denial::InsufficientRole {
                have,
                need: required,
            });
        }
        Ok(())
    }
}

/// Re-reads `tls.allowed_clients` from the configuration file and applies it.
/// On error the current list stays in effect.
pub fn reload_allowed_clients(path: &Path, access: &ClientAccess) -> Result<(), String> {
    let cfg = config::load_config(path).map_err(|e| e.to_string())?;
    let allowed = config::allowed_client_fingerprints(&cfg.tls).map_err(|e| e.to_string())?;
    match &allowed {
        Some(list) => info!("client allow-list reloaded: {} certificate(s)", list.len()),
        None => warn!(
            "client allow-list reloaded: tls.allowed_clients is not set, any client certificate signed by the cluster CA is accepted"
        ),
    }
    access.replace(allowed);
    Ok(())
}

/// Reloads the allow-list from `path` every time the process receives SIGHUP.
#[cfg(unix)]
pub fn spawn_reload_on_sighup(path: std::path::PathBuf, access: ClientAccess) {
    use tokio::signal::unix::{SignalKind, signal};
    tokio::spawn(async move {
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!("failed to install SIGHUP handler: {err}");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            if let Err(err) = reload_allowed_clients(&path, &access) {
                tracing::error!("failed to reload client allow-list, keeping previous list: {err}");
            }
        }
    });
}

#[derive(Serialize)]
struct ErrorBody {
    message: String,
}

fn deny(status: StatusCode, message: String) -> Response {
    (status, Json(ErrorBody { message })).into_response()
}

async fn authorize(
    access: ClientAccess,
    required: ClientRole,
    req: Request,
    next: Next,
) -> Response {
    let Some(identity) = req.extensions().get::<ClientIdentity>().cloned() else {
        return deny(
            StatusCode::UNAUTHORIZED,
            "client certificate required".to_string(),
        );
    };
    match access.check(&identity, required) {
        Ok(()) => next.run(req).await,
        Err(denial) => {
            warn!(
                fingerprint = %identity.fingerprint,
                role = %identity.effective_role(),
                path = %req.uri().path(),
                "client request denied: {}",
                denial.message()
            );
            deny(StatusCode::FORBIDDEN, denial.message())
        }
    }
}

pub async fn require_read(
    State(access): State<ClientAccess>,
    req: Request,
    next: Next,
) -> Response {
    authorize(access, ClientRole::Read, req, next).await
}

pub async fn require_operator(
    State(access): State<ClientAccess>,
    req: Request,
    next: Next,
) -> Response {
    authorize(access, ClientRole::Operator, req, next).await
}

/// Wraps a TLS acceptor and attaches the peer's [`ClientIdentity`] to every request
/// served on the connection.
#[derive(Debug, Clone)]
pub struct ClientIdentityAcceptor<A> {
    inner: A,
}

impl<A> ClientIdentityAcceptor<A> {
    pub fn new(inner: A) -> Self {
        Self { inner }
    }
}

impl<A, I, S, T> Accept<I, S> for ClientIdentityAcceptor<A>
where
    A: Accept<I, S, Stream = TlsStream<T>>,
    A::Future: Send + 'static,
    A::Service: Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = TlsStream<T>;
    type Service = AddExtension<A::Service, ClientIdentity>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let accepted = self.inner.accept(stream, service);
        Box::pin(async move {
            let (stream, service) = accepted.await?;
            let der = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|certs| certs.first())
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "no client certificate")
                })?;
            let identity = ClientIdentity::from_der(der.as_ref());
            Ok((stream, Extension(identity).layer(service)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertificateParams, DnType, KeyPair};

    fn cert_der_with_ous(ous: &[&str]) -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name.push(DnType::CommonName, "client");
        for ou in ous {
            params
                .distinguished_name
                .push(DnType::OrganizationalUnitName, *ou);
        }
        params.self_signed(&key).unwrap().der().to_vec()
    }

    fn identity(fp: &str, role: Option<ClientRole>) -> ClientIdentity {
        ClientIdentity {
            fingerprint: fp.to_string(),
            role,
        }
    }

    #[test]
    fn test_normalize_fingerprint() {
        let colon = (0..32).map(|_| "AB").collect::<Vec<_>>().join(":");
        let expected = colon.to_lowercase();
        assert_eq!(normalize_fingerprint(&colon), Some(expected.clone()));
        assert_eq!(
            normalize_fingerprint(&"ab".repeat(32)),
            Some(expected.clone())
        );
        assert_eq!(
            normalize_fingerprint(&format!("  {expected} ")),
            Some(expected)
        );
        assert_eq!(normalize_fingerprint(""), None);
        assert_eq!(normalize_fingerprint(&"ab".repeat(31)), None);
        assert_eq!(normalize_fingerprint(&"zz".repeat(32)), None);
        assert_eq!(normalize_fingerprint(&"é".repeat(32)), None);
    }

    #[test]
    fn test_fingerprint_matches_normalized_form() {
        let der = cert_der_with_ous(&[]);
        let fp = fingerprint(&der);
        assert_eq!(fp.len(), 95);
        assert_eq!(normalize_fingerprint(&fp.to_uppercase()), Some(fp));
    }

    #[test]
    fn test_role_from_der() {
        assert_eq!(
            role_from_der(&cert_der_with_ous(&["brewmble-role:read"])),
            Some(ClientRole::Read)
        );
        assert_eq!(
            role_from_der(&cert_der_with_ous(&["team-a", "brewmble-role:operator"])),
            Some(ClientRole::Operator)
        );
        assert_eq!(role_from_der(&cert_der_with_ous(&[])), None);
        assert_eq!(role_from_der(&cert_der_with_ous(&["operator"])), None);
        assert_eq!(
            role_from_der(&cert_der_with_ous(&["brewmble-role:admin"])),
            None
        );
        assert_eq!(role_from_der(b"not a certificate"), None);
    }

    #[test]
    fn test_identity_without_role_is_read_only() {
        let id = ClientIdentity::from_der(&cert_der_with_ous(&[]));
        assert_eq!(id.role, None);
        assert_eq!(id.effective_role(), ClientRole::Read);
    }

    #[test]
    fn test_access_without_allow_list_checks_role_only() {
        let access = ClientAccess::default();
        assert!(
            access
                .check(&identity("aa", None), ClientRole::Read)
                .is_ok()
        );
        assert_eq!(
            access.check(&identity("aa", None), ClientRole::Operator),
            Err(Denial::InsufficientRole {
                have: ClientRole::Read,
                need: ClientRole::Operator
            })
        );
        assert!(
            access
                .check(
                    &identity("aa", Some(ClientRole::Operator)),
                    ClientRole::Operator
                )
                .is_ok()
        );
    }

    #[test]
    fn test_access_allow_list_and_revocation() {
        let access = ClientAccess::new(Some(HashSet::from(["aa".to_string(), "bb".to_string()])));
        let operator = identity("aa", Some(ClientRole::Operator));
        let reader = identity("bb", Some(ClientRole::Read));
        let stranger = identity("cc", Some(ClientRole::Operator));

        assert!(access.check(&operator, ClientRole::Operator).is_ok());
        assert!(access.check(&reader, ClientRole::Read).is_ok());
        assert!(matches!(
            access.check(&reader, ClientRole::Operator),
            Err(Denial::InsufficientRole { .. })
        ));
        assert_eq!(
            access.check(&stranger, ClientRole::Read),
            Err(Denial::NotAllowed)
        );

        access.replace(Some(HashSet::from(["bb".to_string()])));
        assert_eq!(
            access.check(&operator, ClientRole::Read),
            Err(Denial::NotAllowed)
        );
        assert!(access.check(&reader, ClientRole::Read).is_ok());

        access.replace(Some(HashSet::new()));
        assert_eq!(
            access.check(&reader, ClientRole::Read),
            Err(Denial::NotAllowed)
        );
    }

    #[test]
    fn test_denial_messages() {
        assert!(Denial::NotAllowed.message().contains("not allowed"));
        let msg = Denial::InsufficientRole {
            have: ClientRole::Read,
            need: ClientRole::Operator,
        }
        .message();
        assert!(msg.contains("'read'") && msg.contains("'operator'"));
    }

    fn write_config(dir: &Path, allowed: &str) -> std::path::PathBuf {
        let path = dir.join("brewmbled.yaml");
        std::fs::write(
            &path,
            format!("tls:\n  ca_cert: /ca.pem\n  cert: /c.pem\n  key: /k.pem\n{allowed}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn test_reload_allowed_clients() {
        let dir = tempfile::tempdir().unwrap();
        let a = "aa".repeat(32);
        let b = "bb".repeat(32);
        let access = ClientAccess::default();
        let reader = |hex: &str| identity(&normalize_fingerprint(hex).unwrap(), None);

        let path = write_config(
            dir.path(),
            &format!("  allowed_clients:\n    - \"{a}\"\n    - \"{b}\"\n"),
        );
        reload_allowed_clients(&path, &access).unwrap();
        assert!(access.check(&reader(&a), ClientRole::Read).is_ok());
        assert!(access.check(&reader(&b), ClientRole::Read).is_ok());

        let path = write_config(dir.path(), &format!("  allowed_clients:\n    - \"{b}\"\n"));
        reload_allowed_clients(&path, &access).unwrap();
        assert_eq!(
            access.check(&reader(&a), ClientRole::Read),
            Err(Denial::NotAllowed)
        );

        // An invalid file keeps the previous list.
        let path = write_config(dir.path(), "  allowed_clients:\n    - \"nope\"\n");
        assert!(
            reload_allowed_clients(&path, &access)
                .unwrap_err()
                .contains("nope")
        );
        assert_eq!(
            access.check(&reader(&a), ClientRole::Read),
            Err(Denial::NotAllowed)
        );
        assert!(access.check(&reader(&b), ClientRole::Read).is_ok());

        std::fs::write(&path, "not: [valid").unwrap();
        assert!(reload_allowed_clients(&path, &access).is_err());
        assert!(access.check(&reader(&b), ClientRole::Read).is_ok());

        let path = write_config(dir.path(), "");
        reload_allowed_clients(&path, &access).unwrap();
        assert!(access.check(&reader(&a), ClientRole::Read).is_ok());
    }
}
