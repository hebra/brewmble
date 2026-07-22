# Brewmble Architecture

This document describes the high-level architecture of Brewmble, its components, and how they interact.

## Component Overview

Brewmble is designed as a distributed system consisting of a central controller (CLI) and multiple managed agents (Daemons).

### 1. Brewmble Daemon (`brewmbled`)
The daemon is a lightweight agent that runs on each node you want to manage.
- **Responsibilities**:
    - Abstracting the local package manager (APT on Linux, Homebrew on macOS).
    - Exposing a REST API for status checks and upgrade triggers.
    - Registering itself on the network via mDNS for automatic discovery.
- **Technology Stack**: Rust, Axum (Web Framework), `mdns-sd` (Service Discovery), `tokio` (Async Runtime).

### 2. Brewmble CLI (`brewmble`)
The CLI is the primary interface for users to interact with the managed fleet.
- **Responsibilities**:
    - Discovering active daemons on the local network.
    - Aggregating status information from multiple nodes.
    - Orchestrating updates across selected targets.
    - Managing local configuration and API keys.
- **Technology Stack**: Rust, `reqwest` (HTTP Client), `clap` (CLI Parser), `mdns-sd`.

### 3. Brewmble REST (Crate)
Shared Rust library located in `rest/` and used by both the CLI and the daemon.
- **Responsibilities**:
    - Shared data models for API requests and responses.
    - Common error types, endpoint paths, and service-discovery constants.

---

## Service Discovery

Brewmble uses **mDNS (Multicast DNS)** for zero-configuration discovery.

- **Service Type**: `_brewmble._tcp.local.`
- **Instance Name**: `brewmbled-{hostname}`
- **Discovery Flow**:
    1. At startup, `brewmbled` registers itself with the mDNS daemon on the local network.
    2. When the CLI runs a `discover` or `status --all` command, it browses for `_brewmble._tcp.local.` services.
    3. The CLI collects the IP addresses and ports of all responding daemons.

---

## Communication & API

Communication between the CLI and Daemon occurs over HTTP/S using a RESTful API.

### Authentication
Security is handled via a simple API Key mechanism.
- **Header**: `X-API-Key`
- **Behavior**: The daemon requires this header for all non-discovery requests. If the key is missing or incorrect, it returns `401 Unauthorized`.
- **Key Generation**: If no key is provided via `BREWMBLE_DAEMON_API_KEY`, the daemon generates a random UUID v4 and logs it at startup.

### Endpoints
- `GET /status`: Returns information about the system and available updates.
- `POST /packages/full-upgrade`: Triggers a system-wide upgrade. This is a fire-and-forget operation in the background to prevent HTTP timeouts.

---

## Data Flow

### Status Check Flow
1. User executes `brewmble status --all`.
2. CLI browses mDNS to find all `brewmbled` instances.
3. For each discovered instance, the CLI sends a `GET /status` request (concurrently).
4. Each `brewmbled` queries the local package manager. On Linux, it runs `apt-get update` only if the cache is older than the configured `BREWMBLE_APT_UPDATE_INTERVAL`. It then runs `apt-get -s dist-upgrade` to determine available updates.
5. `brewmbled` returns a JSON response with the update count and details.
6. CLI aggregates and displays the results in a formatted table.

### Upgrade Flow
1. User executes `brewmble packages --full-upgrade <target>`.
2. CLI sends a `POST /packages/full-upgrade` request to the target daemon with a JSON body: `{"dry_run": false}`.
3. Daemon verifies the API key.
4. If `dry_run` is `true`, the daemon simulates the upgrade and returns the list of packages that would be changed.
5. If `dry_run` is `false`, the daemon sets an `is_upgrading` flag (AtomicBool) to prevent concurrent upgrades.
6. Daemon spawns a background task to perform the upgrade (e.g., `apt-get dist-upgrade -y`).
7. Daemon immediately returns `200 OK` (with a "triggered" message) to the CLI.
8. The CLI informs the user that the upgrade has started.

---

## Security Model

1. **Local Network Trust**: Brewmble assumes a relatively trusted local network for mDNS.
2. **API Keys**: Protects against unauthorized control if the daemon port is exposed.
3. **Transport Security**: While the daemon serves HTTP by default, it is designed to be placed behind a reverse proxy (like Caddy or Nginx) for TLS termination.
4. **Least Privilege**: The daemon requires sufficient permissions to run package management commands (often requiring `sudo` or running as `root`).

---

## Platform Specifics

- **Linux**: Uses CLI commands like `apt-get`. Requires a Debian-based system.
- **macOS**: Uses Homebrew (`brew`) via the implementation in `daemon/src/package_manager/brew.rs`.
- **Conditional Compilation**: Extensive use of `#[cfg(target_os = "linux")]` and `#[cfg(target_os = "macos")]` to handle package manager differences.
