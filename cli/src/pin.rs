use crate::ca;
use rustls::{
    ClientConfig, RootCertStore,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
struct CapturingVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    leaf: Mutex<Option<Vec<u8>>>,
}

impl CapturingVerifier {
    fn new(roots: RootCertStore) -> Result<Self, Box<dyn Error>> {
        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(roots)).build()?;
        Ok(Self {
            inner,
            leaf: Mutex::new(None),
        })
    }

    fn take_leaf(&self) -> Option<Vec<u8>> {
        self.leaf.lock().unwrap().take()
    }
}

impl ServerCertVerifier for CapturingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let result = self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        )?;
        *self.leaf.lock().unwrap() = Some(end_entity.as_ref().to_vec());
        Ok(result)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

pub fn fetch_fingerprint(url: &str) -> Result<String, Box<dyn Error>> {
    let ca_pem = ca::ca_root_pem()?;
    let mut ca_reader = std::io::BufReader::new(ca_pem.as_bytes());
    let ca_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut ca_reader).collect::<Result<Vec<_>, _>>()?;
    let mut roots = RootCertStore::empty();
    for cert in ca_certs {
        roots.add(cert)?;
    }

    let verifier = Arc::new(CapturingVerifier::new(roots)?);
    let client_cert = load_client_cert()?;

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_client_auth_cert(client_cert.0, client_cert.1)?;

    let (host, port) = parse_host_port(url)?;
    let server_name = ServerName::try_from(host.clone())?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)?;
    let mut sock = TcpStream::connect((host.as_str(), port))?;
    let mut tls = rustls::Stream::new(&mut conn, &mut sock);
    tls.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let _ = tls.flush();
    let mut buf = [0u8; 1];
    let _ = tls.read(&mut buf);

    let leaf = verifier.take_leaf().ok_or("no leaf certificate captured")?;
    Ok(format_fingerprint(&leaf))
}

fn load_client_cert()
-> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), Box<dyn Error>> {
    let cert_pem = ca::client_cert_pem()?;
    let key_pem = ca::client_key_pem()?;

    let mut cert_reader = std::io::BufReader::new(cert_pem.as_bytes());
    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    let mut key_reader = std::io::BufReader::new(key_pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or("no client private key found in keyring")?;

    Ok((certs, key))
}

fn parse_host_port(url: &str) -> Result<(String, u16), Box<dyn Error>> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("url must use https scheme")?;

    if let Some(inner) = rest.strip_prefix('[') {
        let (host, port_str) = inner
            .split_once("]:")
            .map(|(h, p)| (format!("[{}]", h), p))
            .or_else(|| inner.strip_suffix(']').map(|h| (format!("[{}]", h), "443")))
            .ok_or("invalid IPv6 URL")?;
        let port = port_str.parse()?;
        return Ok((host, port));
    }

    let (host, port_str) = rest.split_once(':').unwrap_or((rest, "443"));
    let port = port_str.parse()?;
    Ok((host.to_string(), port))
}

fn format_fingerprint(der: &[u8]) -> String {
    let hash = Sha256::digest(der);
    hash.iter()
        .map(|b| format!("{:02x}", b))
        .collect::<Vec<_>>()
        .join(":")
}

#[derive(Debug)]
pub enum PinOutcome {
    Learned(String),
    Matched,
}

pub fn check_pin(node: &crate::NodeConfig, address: &str) -> Result<PinOutcome, Box<dyn Error>> {
    check_pin_with_fetcher(node, address, fetch_fingerprint)
}

fn check_pin_with_fetcher(
    node: &crate::NodeConfig,
    address: &str,
    fetch: impl FnOnce(&str) -> Result<String, Box<dyn Error>>,
) -> Result<PinOutcome, Box<dyn Error>> {
    let url = crate::resolve_url(address);
    let observed = fetch(&url)?;

    match &node.fingerprint {
        None => Ok(PinOutcome::Learned(observed)),
        Some(expected) => {
            if expected.eq_ignore_ascii_case(&observed) {
                Ok(PinOutcome::Matched)
            } else {
                Err(format!(
                    "fingerprint mismatch for {}: expected {}, got {}; run 'brewmble node --repin {}' to accept the new identity",
                    node_name(node),
                    expected,
                    observed,
                    node_name(node),
                )
                .into())
            }
        }
    }
}

pub(crate) fn node_name(node: &crate::NodeConfig) -> &str {
    node.name.as_deref().unwrap_or(&node.address)
}

impl fmt::Display for PinOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PinOutcome::Learned(fp) => write!(f, "learned fingerprint {}", fp),
            PinOutcome::Matched => write!(f, "fingerprint matched"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_host_port_ipv4_with_port() {
        assert_eq!(
            parse_host_port("https://1.2.3.4:8080").unwrap(),
            ("1.2.3.4".to_string(), 8080)
        );
    }

    #[test]
    fn test_parse_host_port_ipv4_default() {
        assert_eq!(
            parse_host_port("https://1.2.3.4").unwrap(),
            ("1.2.3.4".to_string(), 443)
        );
    }

    #[test]
    fn test_parse_host_port_ipv6_with_port() {
        assert_eq!(
            parse_host_port("https://[::1]:8080").unwrap(),
            ("[::1]".to_string(), 8080)
        );
    }

    #[test]
    fn test_parse_host_port_ipv6_default() {
        assert_eq!(
            parse_host_port("https://[::1]").unwrap(),
            ("[::1]".to_string(), 443)
        );
    }

    #[test]
    fn test_format_fingerprint() {
        let fp = format_fingerprint(b"hello");
        assert_eq!(fp.len(), 95); // 32 bytes -> 64 hex chars + 31 colons
        assert!(fp.contains(':'));
    }

    #[test]
    fn test_check_pin_learns() {
        let node = crate::NodeConfig {
            name: Some("node1".to_string()),
            address: "1.2.3.4:4712".to_string(),
            fingerprint: None,
        };
        let outcome =
            check_pin_with_fetcher(&node, &node.address, |_| Ok("aa:bb".to_string())).unwrap();
        assert!(matches!(outcome, PinOutcome::Learned(fp) if fp == "aa:bb"));
    }

    #[test]
    fn test_check_pin_matches() {
        let node = crate::NodeConfig {
            name: Some("node1".to_string()),
            address: "1.2.3.4:4712".to_string(),
            fingerprint: Some("AA:BB".to_string()),
        };
        let outcome =
            check_pin_with_fetcher(&node, &node.address, |_| Ok("aa:bb".to_string())).unwrap();
        assert!(matches!(outcome, PinOutcome::Matched));
    }

    #[test]
    fn test_check_pin_mismatch_message() {
        let node = crate::NodeConfig {
            name: Some("node1".to_string()),
            address: "1.2.3.4:4712".to_string(),
            fingerprint: Some("aa:bb".to_string()),
        };
        let err =
            check_pin_with_fetcher(&node, &node.address, |_| Ok("cc:dd".to_string())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("fingerprint mismatch for node1"));
        assert!(msg.contains("expected aa:bb"));
        assert!(msg.contains("got cc:dd"));
        assert!(msg.contains("brewmble node --repin node1"));
    }
}
