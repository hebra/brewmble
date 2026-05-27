use serde::{Deserialize, Serialize};

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
