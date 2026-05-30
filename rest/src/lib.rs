use serde::{Deserialize, Serialize};

pub const API_KEY_HEADER: &str = "X-API-Key";
pub const SERVICE_TYPE: &str = "_brewmble._tcp";
pub const SERVICE_DOMAIN: &str = "local.";
pub const SERVICE_FULL_TYPE: &str = "_brewmble._tcp.local.";

pub const PATH_STATUS: &str = "/status";
pub const PATH_HEALTH: &str = "/health";
pub const PATH_UPGRADE: &str = "/packages/full-upgrade";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StatusResponse {
    pub message: String,
    pub updates: Vec<String>,
    pub is_upgrading: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HealthResponse {
    pub status: String,
    pub package_manager: String,
    pub package_manager_version: String,
    pub is_upgrading: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct UpgradeRequest {
    pub dry_run: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpgradeResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updates: Option<Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_response_serialization() {
        let resp = StatusResponse {
            message: "All good".to_string(),
            updates: vec!["pkg1".to_string(), "pkg2".to_string()],
            is_upgrading: false,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"message\":\"All good\""));
        assert!(json.contains("\"updates\":[\"pkg1\",\"pkg2\"]"));
        assert!(json.contains("\"is_upgrading\":false"));

        let decoded: StatusResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.message, resp.message);
        assert_eq!(decoded.updates, resp.updates);
        assert_eq!(decoded.is_upgrading, resp.is_upgrading);
    }

    #[test]
    fn test_health_response_serialization() {
        let resp = HealthResponse {
            status: "ok".to_string(),
            package_manager: "apt".to_string(),
            package_manager_version: "1.2.3".to_string(),
            is_upgrading: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"status\":\"ok\""));
        assert!(json.contains("\"package_manager\":\"apt\""));
        assert!(json.contains("\"package_manager_version\":\"1.2.3\""));
        assert!(json.contains("\"is_upgrading\":true"));

        let decoded: HealthResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.status, resp.status);
        assert_eq!(decoded.package_manager, resp.package_manager);
        assert_eq!(decoded.package_manager_version, resp.package_manager_version);
        assert_eq!(decoded.is_upgrading, resp.is_upgrading);
    }

    #[test]
    fn test_upgrade_response_serialization() {
        let resp = UpgradeResponse {
            message: "Upgrade started".to_string(),
            updates: Some(vec!["pkg1".to_string()]),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"message\":\"Upgrade started\""));
        assert!(json.contains("\"updates\":[\"pkg1\"]"));

        let decoded: UpgradeResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.message, resp.message);
        assert_eq!(decoded.updates, resp.updates);
    }

    #[test]
    fn test_upgrade_request_serialization() {
        let req = UpgradeRequest { dry_run: true };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"dry_run\":true"));

        let decoded: UpgradeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.dry_run, req.dry_run);
    }
}
