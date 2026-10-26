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
use std::net::{TcpStream, ToSocketAddrs};
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
    let client_cert = load_client_cert()?;
    fetch_fingerprint_with_certs(url, &ca_pem, client_cert, crate::get_default_timeout())
}

fn fetch_fingerprint_with_certs(
    url: &str,
    ca_pem: &str,
    client_cert: (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>),
    timeout: std::time::Duration,
) -> Result<String, Box<dyn Error>> {
    let (server_name, addrs) = resolve_url_addresses(url)?;
    fetch_fingerprint_with_addrs(server_name, addrs, ca_pem, client_cert, timeout)
}

fn resolve_url_addresses(
    url: &str,
) -> Result<(ServerName<'static>, Vec<std::net::SocketAddr>), Box<dyn Error>> {
    let (host, port) = parse_host_port(url)?;
    // IPv6 URLs use brackets; strip them for DNS/IP resolution and SNI.
    let resolution_host = host.trim_start_matches('[').trim_end_matches(']');
    let server_name = ServerName::try_from(resolution_host.to_string())?;
    let addrs: Vec<std::net::SocketAddr> = (resolution_host, port)
        .to_socket_addrs()?
        .collect();
    if addrs.is_empty() {
        return Err(format!("could not resolve {}:{}", host, port).into());
    }
    Ok((server_name, addrs))
}

fn fetch_fingerprint_with_addrs(
    server_name: ServerName<'static>,
    addrs: Vec<std::net::SocketAddr>,
    ca_pem: &str,
    client_cert: (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>),
    timeout: std::time::Duration,
) -> Result<String, Box<dyn Error>> {
    let mut ca_reader = std::io::BufReader::new(ca_pem.as_bytes());
    let ca_certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut ca_reader).collect::<Result<Vec<_>, _>>()?;
    let mut roots = RootCertStore::empty();
    for cert in ca_certs {
        roots.add(cert)?;
    }

    let verifier = Arc::new(CapturingVerifier::new(roots)?);

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone())
        .with_client_auth_cert(client_cert.0, client_cert.1)?;

    let config = Arc::new(config);
    let mut last_err: Option<Box<dyn Error>> = None;
    for addr in addrs {
        match try_fetch_at_address(addr, server_name.clone(), config.clone(), verifier.clone(), timeout) {
            Ok(fp) => return Ok(fp),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| "all addresses failed".into()))
}

fn try_fetch_at_address(
    addr: std::net::SocketAddr,
    server_name: ServerName<'static>,
    config: Arc<ClientConfig>,
    verifier: Arc<CapturingVerifier>,
    timeout: std::time::Duration,
) -> Result<String, Box<dyn Error>> {
    let sock = TcpStream::connect_timeout(&addr, timeout)?;

    // Run the TLS handshake in a worker thread and enforce a wall-clock
    // timeout. rustls's Stream retries on WouldBlock, so socket-level
    // read/write timeouts are not sufficient on their own.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut conn = match rustls::ClientConnection::new(config, server_name) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(format!("tls init: {e}")));
                return;
            }
        };
        let mut stream = sock;
        let mut tls = rustls::Stream::new(&mut conn, &mut stream);
        let result = tls
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .and_then(|()| tls.flush())
            .and_then(|()| {
                let mut buf = [0u8; 1];
                tls.read(&mut buf).map(|_| ())
            });
        let _ = tx.send(result.map_err(|e| format!("tls handshake: {e}")));
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => return Err(format!("pin check timed out after {:?}", timeout).into()),
    }

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

    fn test_certs() -> (String, String, String, String, String) {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
            PKCS_ECDSA_P256_SHA256, SanType,
        };
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.distinguished_name.push(DnType::CommonName, "test-ca");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let issuer = rcgen::Issuer::new(
            {
                let mut p = CertificateParams::new(Vec::<String>::new()).unwrap();
                p.distinguished_name.push(DnType::CommonName, "test-ca");
                p.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
                p
            },
            ca_key,
        );

        let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut server_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        server_params.distinguished_name.push(DnType::CommonName, "test-server");
        server_params.subject_alt_names = vec![SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        ))];
        server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server_cert = server_params.signed_by(&server_key, &issuer).unwrap();

        let client_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        client_params.distinguished_name.push(DnType::CommonName, "test-client");
        client_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client_cert = client_params.signed_by(&client_key, &issuer).unwrap();

        (
            ca_cert.pem(),
            server_cert.pem(),
            server_key.serialize_pem(),
            client_cert.pem(),
            client_key.serialize_pem(),
        )
    }

    fn run_tls_server(
        ca_pem: &str,
        server_cert_pem: &str,
        server_key_pem: &str,
    ) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let mut cert_reader = std::io::BufReader::new(server_cert_pem.as_bytes());
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>().unwrap();
        let mut key_reader = std::io::BufReader::new(server_key_pem.as_bytes());
        let key = rustls_pemfile::private_key(&mut key_reader).unwrap().unwrap();

        let mut roots = RootCertStore::empty();
        let mut ca_reader = std::io::BufReader::new(ca_pem.as_bytes());
        for cert in rustls_pemfile::certs(&mut ca_reader) {
            roots.add(cert.unwrap()).unwrap();
        }

        let config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                    .build()
                    .unwrap(),
            )
            .with_single_cert(certs, key)
            .unwrap();
        let config = Arc::new(config);

        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut conn = rustls::ServerConnection::new(config).unwrap();
            let mut tls = rustls::Stream::new(&mut conn, &mut stream);
            let mut buf = [0u8; 1024];
            let _ = tls.read(&mut buf);
            let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            let _ = tls.flush();
        });

        addr
    }

    fn load_client_cert_from_pem(
        cert_pem: &str,
        key_pem: &str,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let mut cert_reader = std::io::BufReader::new(cert_pem.as_bytes());
        let certs: Vec<CertificateDer<'static>> =
            rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>().unwrap();
        let mut key_reader = std::io::BufReader::new(key_pem.as_bytes());
        let key = rustls_pemfile::private_key(&mut key_reader).unwrap().unwrap();
        (certs, key)
    }

    #[test]
    fn test_fetch_fingerprint_times_out_on_stalled_handshake() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for s in listener.incoming().flatten() {
                // Accept the connection but never send any data back.
                std::thread::sleep(std::time::Duration::from_secs(30));
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        });

        let (ca_pem, _, _, client_cert_pem, client_key_pem) = test_certs();
        let (certs, key) = load_client_cert_from_pem(&client_cert_pem, &client_key_pem);

        let timeout = std::time::Duration::from_secs(1);
        let start = std::time::Instant::now();
        let result = fetch_fingerprint_with_certs(
            &format!("https://127.0.0.1:{}", port),
            &ca_pem,
            (certs, key),
            timeout,
        );
        let elapsed = start.elapsed();

        drop(handle);

        assert!(result.is_err(), "expected error from stalled handshake");
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "timed out but took too long: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_resolve_url_addresses_ipv4() {
        let (server_name, addrs) = resolve_url_addresses("https://127.0.0.1:4712").unwrap();
        assert_eq!(addrs.len(), 1);
        assert!(addrs[0].is_ipv4());
        assert!(matches!(server_name, ServerName::IpAddress(_)));
    }

    #[test]
    fn test_resolve_url_addresses_ipv6() {
        let (server_name, addrs) = resolve_url_addresses("https://[::1]:4712").unwrap();
        assert_eq!(addrs.len(), 1);
        assert!(addrs[0].is_ipv6());
        assert!(matches!(server_name, ServerName::IpAddress(_)));
    }

    #[test]
    fn test_fetch_fingerprint_fallback_to_second_address() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (ca_pem, server_cert_pem, server_key_pem, client_cert_pem, client_key_pem) =
            test_certs();
        let server_addr = run_tls_server(&ca_pem, &server_cert_pem, &server_key_pem);
        let port = server_addr.port();
        let (certs, key) = load_client_cert_from_pem(&client_cert_pem, &client_key_pem);

        let server_name = ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let addrs = vec![
            std::net::SocketAddr::new(
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
                port,
            ),
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port,
            ),
        ];

        let result = fetch_fingerprint_with_addrs(
            server_name,
            addrs,
            &ca_pem,
            (certs, key),
            std::time::Duration::from_secs(5),
        );
        assert!(
            result.is_ok(),
            "expected fallback to IPv4 to succeed: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap().len(), 95);
    }

    #[test]
    fn test_fetch_fingerprint_fails_when_all_addresses_fail() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (ca_pem, _, _, client_cert_pem, client_key_pem) = test_certs();
        let (certs, key) = load_client_cert_from_pem(&client_cert_pem, &client_key_pem);

        let server_name = ServerName::try_from("127.0.0.1".to_string()).unwrap();
        let addrs = vec![std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            1,
        )];

        let result = fetch_fingerprint_with_addrs(
            server_name,
            addrs,
            &ca_pem,
            (certs, key),
            std::time::Duration::from_secs(1),
        );
        assert!(result.is_err());
    }
}
