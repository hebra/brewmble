# Cobbler Architecture

This document describes the high-level architecture of Cobbler, its components, and how they interact.

## Component Overview

Cobbler is designed as a distributed system consisting of a central controller (CLI) and multiple managed agents (Daemons).

### 1. Cobbler Daemon (`cobblerd`)
The daemon is a lightweight agent that runs on each node you want to manage.
- **Responsibilities**:
    - Abstracting the local package manager (APT on Linux, Homebrew on macOS).
    - Exposing a REST API for status checks and upgrade triggers.
    - Registering itself on the network via mDNS for automatic discovery.
- **Technology Stack**: Rust, Axum (Web Framework), `mdns-sd` (Service Discovery), `tokio` (Async Runtime).

### 2. Cobbler CLI (`cobbler`)
The CLI is the primary interface for users to interact with the managed fleet.
- **Responsibilities**:
    - Discovering active daemons on the local network.
    - Aggregating status information from multiple nodes.
    - Orchestrating updates across selected targets.
    - Managing local configuration and API keys.
- **Technology Stack**: Rust, `reqwest` (HTTP Client), `clap` (CLI Parser), `mdns-sd`.

### 3. Cobbler REST (Crate)
*Note: Currently integrated within components, but planned to be extracted into a shared crate.*
- **Responsibilities**:
    - Shared data models for API requests and responses.
    - Common error types and constants.

---

## Service Discovery

Cobbler uses **mDNS (Multicast DNS)** for zero-configuration discovery.

- **Service Type**: `_cobbler._tcp.local.`
- **Instance Name**: `cobblerd-{hostname}`
- **Discovery Flow**:
    1. At startup, `cobblerd` registers itself with the mDNS daemon on the local network.
    2. When the CLI runs a `discover` or `status --all` command, it browses for `_cobbler._tcp.local.` services.
    3. The CLI collects the IP addresses and ports of all responding daemons.

---

## Communication & API

Communication between the CLI and Daemon occurs over HTTP/S using a RESTful API.

### Authentication
Security is handled via a simple API Key mechanism.
- **Header**: `X-API-Key`
- **Behavior**: The daemon requires this header for all non-discovery requests. If the key is missing or incorrect, it returns `401 Unauthorized`.
- **Key Generation**: If no key is provided via `COBBLER_DAEMON_API_KEY`, the daemon generates a random UUID v4 and logs it at startup.

### Endpoints
- `GET /status`: Returns information about the system and available updates.
- `POST /packages/full-upgrade`: Triggers a system-wide upgrade. This is a fire-and-forget operation in the background to prevent HTTP timeouts.

---

## Data Flow

### Status Check Flow
1. User executes `cobbler status --all`.
2. CLI browses mDNS to find all `cobblerd` instances.
3. For each discovered instance, the CLI sends a `GET /status` request (concurrently).
4. Each `cobblerd` queries the local package manager (e.g., `apt-get update && apt-get upgrade --simulate`).
5. `cobblerd` returns a JSON response with the update count and details.
6. CLI aggregates and displays the results in a formatted table.

### Upgrade Flow
1. User executes `cobbler packages --full-upgrade <target>`.
2. CLI sends a `POST /packages/full-upgrade` request to the target daemon.
3. Daemon verifies the API key.
4. Daemon sets an `is_upgrading` flag (AtomicBool) to prevent concurrent upgrades.
5. Daemon spawns a background task to perform the upgrade (e.g., `apt-get dist-upgrade -y`).
6. Daemon immediately returns `202 Accepted` to the CLI.
7. The CLI informs the user that the upgrade has started.

---

## Security Model

1. **Local Network Trust**: Cobbler assumes a relatively trusted local network for mDNS.
2. **API Keys**: Protects against unauthorized control if the daemon port is exposed.
3. **Transport Security**: While the daemon serves HTTP by default, it is designed to be placed behind a reverse proxy (like Caddy or Nginx) for TLS termination.
4. **Least Privilege**: The daemon requires sufficient permissions to run package management commands (often requiring `sudo` or running as `root`).

---

## Platform Specifics

- **Linux**: Uses `apt-pkg-native` or executes `apt-get` commands. Requires a Debian-based system.
- **macOS**: Planned support for Homebrew (`brew`).
- **Conditional Compilation**: Extensive use of `#[cfg(target_os = "linux")]` and `#[cfg(target_os = "macos")]` to handle package manager differences.
