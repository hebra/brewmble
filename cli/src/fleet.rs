use crate::{NodeConfig, pin::node_name};
use reqwest::StatusCode;
use reqwest::blocking::{RequestBuilder, Response};
use serde::{Serialize, de::DeserializeOwned};
use std::io::{self, Write};
use std::process::ExitCode;

pub(crate) struct NodeReport<T> {
    pub name: String,
    pub address: String,
    pub http_status: Option<StatusCode>,
    pub outcome: Result<T, String>,
}

impl<T: DeserializeOwned> NodeReport<T> {
    pub fn request(node: &NodeConfig, request: RequestBuilder) -> Self {
        match request.send() {
            Ok(response) => Self::from_response(node, response),
            Err(error) => Self::failed(node, format!("Error: {error}")),
        }
    }

    fn from_response(node: &NodeConfig, response: Response) -> Self {
        let status = response.status();
        let outcome = if status.is_success() {
            response
                .json()
                .map_err(|error| format!("Could not parse response: {error}"))
        } else {
            Err(match response.json::<serde_json::Value>() {
                Ok(body) => format!("HTTP {status}: {body}"),
                Err(_) => format!("HTTP {status}"),
            })
        };
        Self::new(node, Some(status), outcome)
    }
}

impl<T> NodeReport<T> {
    fn new(node: &NodeConfig, http_status: Option<StatusCode>, outcome: Result<T, String>) -> Self {
        Self {
            name: node_name(node).to_string(),
            address: node.address.clone(),
            http_status,
            outcome,
        }
    }

    pub fn failed(node: &NodeConfig, error: String) -> Self {
        Self::new(node, None, Err(error))
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> NodeReport<U> {
        NodeReport {
            name: self.name,
            address: self.address,
            http_status: self.http_status,
            outcome: self.outcome.map(f),
        }
    }
}

pub(crate) fn exit_code<T>(reports: &[NodeReport<T>]) -> ExitCode {
    if reports.iter().any(|report| report.outcome.is_err()) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

pub(crate) fn write_json<T: Serialize>(
    mut writer: impl Write,
    reports: &[NodeReport<T>],
) -> io::Result<()> {
    #[derive(Serialize)]
    struct JsonReport<'a, T> {
        name: &'a str,
        address: &'a str,
        success: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<&'a T>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    }

    let reports: Vec<_> = reports
        .iter()
        .map(|report| JsonReport {
            name: &report.name,
            address: &report.address,
            success: report.outcome.is_ok(),
            http_status: report.http_status.map(|status| status.as_u16()),
            result: report.outcome.as_ref().ok(),
            error: report.outcome.as_ref().err().map(String::as_str),
        })
        .collect();
    serde_json::to_writer(&mut writer, &reports)?;
    writeln!(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Duration;

    fn node(name: Option<&str>, address: &str) -> NodeConfig {
        NodeConfig {
            name: name.map(str::to_owned),
            address: address.to_owned(),
            fingerprint: None,
        }
    }

    #[test]
    fn exit_code_tracks_any_failure() {
        let good = || NodeReport::new(&node(None, "one:4712"), Some(StatusCode::OK), Ok(()));
        let bad = || NodeReport::failed(&node(None, "two:4712"), "timed out".to_owned());
        assert_eq!(exit_code::<()>(&[]), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[good(), good()]), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[good(), bad()]), ExitCode::FAILURE);
        assert_eq!(exit_code(&[bad(), bad()]), ExitCode::FAILURE);
    }

    #[test]
    fn writes_success_and_failure_to_the_same_json_array() {
        let reports = [
            NodeReport::new(
                &node(Some("workstation"), "one:4712"),
                Some(StatusCode::OK),
                Ok(json!({"updates": ["example"]})),
            ),
            NodeReport::failed(&node(None, "two:4712"), "connection failed".to_owned()),
        ];
        let mut output = Vec::new();
        write_json(&mut output, &reports).unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value,
            json!([
                {
                    "name": "workstation",
                    "address": "one:4712",
                    "success": true,
                    "http_status": 200,
                    "result": {"updates": ["example"]}
                },
                {
                    "name": "two:4712",
                    "address": "two:4712",
                    "success": false,
                    "error": "connection failed"
                }
            ])
        );
    }

    #[test]
    fn request_classifies_http_rejections_and_invalid_success_bodies_as_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            for (status, body) in [
                ("200 OK", r#"{"message":"accepted"}"#),
                ("403 Forbidden", r#"{"message":"not permitted"}"#),
                ("200 OK", "not JSON"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0; 1];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        let node = node(None, &address);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let get = || {
            NodeReport::<brewmble_rest::RebootResponse>::request(
                &node,
                client.get(format!("http://{address}/")),
            )
        };
        assert_eq!(get().outcome.unwrap().message, "accepted");
        let rejected = get();
        assert_eq!(rejected.http_status, Some(StatusCode::FORBIDDEN));
        assert!(rejected.outcome.unwrap_err().contains("not permitted"));
        assert!(
            get()
                .outcome
                .unwrap_err()
                .contains("Could not parse response")
        );
        server.join().unwrap();
        let unavailable = get();
        assert!(unavailable.http_status.is_none());
        assert!(unavailable.outcome.is_err());
    }
}
