# Brewmble REST

A shared Rust crate (`brewmble-rest`) used by both the Brewmble CLI and the Brewmble daemon.

It provides:

- **Data models**: Request/response types for the daemon API (`StatusResponse`, `HealthResponse`, `UpgradeRequest` / `UpgradeResponse`, `RebootRequest` / `RebootResponse`).
- **Endpoint paths**: Constants for the API routes (`/health`, `/status`, `/packages/full-upgrade`, `/node/reboot`).
- **Service discovery constants**: The mDNS service type (`_brewmble._tcp`), domain, and TXT property names/values used for automatic daemon discovery.

Keeping these definitions in one crate guarantees that the CLI and the daemon always speak the same protocol.
