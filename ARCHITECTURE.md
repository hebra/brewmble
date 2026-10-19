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
    - Orchestrating updates and reboots across selected targets.
    - Managing configuration profiles and the cluster CA / client certificates (in the OS keyring).
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
- **TXT Properties**: `id={hostname}` and `tls=mtls`
- **Discovery Flow**:
    1. At startup, `brewmbled` registers itself with the mDNS daemon on the local network.
    2. When the CLI runs a `discover` or `status --all` command, it browses for `_brewmble._tcp.local.` services.
    3. The CLI collects the IP addresses and ports of all responding daemons.

---

## Communication & API

Communication between the CLI and Daemon occurs over HTTPS using a RESTful API served directly by the daemon (rustls).

### Authentication
Security is handled exclusively via mutual TLS (mTLS); there are no API keys.
- **Cluster CA**: The CLI manages a single self-signed cluster CA stored in the OS keyring (`brewmble ca init`).
- **Daemon**: Reads the paths to the cluster CA certificate, its own certificate, and its private key from the `tls:` section of `brewmbled.yaml`. It serves HTTPS and requires a client certificate signed by the cluster CA.
- **CLI**: Uses its own client certificate from the keyring (`brewmble cert issue --kind client`) to connect to every daemon. Hostname verification is disabled; only the CA chain is checked.
- **Behavior**: If the client cannot present a valid certificate, the TLS handshake fails before any HTTP request is made.

### Endpoints
- `GET /health`: Returns liveness information and the detected package manager.
- `GET /status`: Returns information about the system and available updates.
- `POST /packages/full-upgrade`: Triggers a system-wide upgrade (or a dry-run). This is a fire-and-forget operation in the background to prevent HTTP timeouts.
- `POST /node/reboot`: Reboots the host, gated by the `features.allow_reboot` configuration flag.

---

## Data Flow

### Status Check Flow
1. User executes `brewmble status --all`.
2. CLI browses mDNS to find all `brewmbled` instances.
3. For each discovered instance, the CLI sends a `GET /status` request (concurrently).
4. Each `brewmbled` queries the local package manager. On Linux, it runs `apt-get update` (on macOS, `brew update`) only if the cache is older than the configured `package_manager.update_interval_mins` (default: 360 minutes). It then runs `apt-get -s dist-upgrade` (or the Homebrew equivalent) to determine available updates.
5. `brewmbled` returns a JSON response with the update count and details.
6. CLI aggregates and displays the results in a formatted table.

### Upgrade Flow
1. User executes `brewmble node --full-upgrade <node>`.
2. CLI sends a `POST /packages/full-upgrade` request to the target daemon with a JSON body: `{"dry_run": false}`.
3. The TLS handshake authenticates the CLI's client certificate against the cluster CA (no request-level authentication takes place).
4. If `dry_run` is `true`, the daemon simulates the upgrade and returns the list of packages that would be changed.
5. If `dry_run` is `false`, the daemon sets an `is_upgrading` flag (AtomicBool) to prevent concurrent upgrades.
6. Daemon spawns a background task to perform the upgrade (e.g., `apt full-upgrade -y` or `brew upgrade`), optionally followed by `autoclean`/`autoremove` cleanup steps.
7. Daemon immediately returns `200 OK` (with a "triggered" message) to the CLI.
8. The CLI informs the user that the upgrade has started.

---

## Security Model

1. **Local Network Trust**: Brewmble assumes a relatively trusted local network for mDNS. Discovery only reveals hostnames and ports; it carries no credentials.
2. **Mutual TLS**: Both transport encryption and authentication are built-in. Only clients presenting a certificate signed by the cluster CA can connect, and the daemon's identity is anchored to the same CA.
3. **Key Management**: The cluster CA and the CLI client certificate live exclusively in the OS keyring. Daemon certificates are private to each node and read via the `tls:` configuration section.
4. **Least Privilege**: The daemon requires sufficient permissions to run package management commands (via `sudo` for APT on Linux; Homebrew operations run as the service user).

---

## Platform Specifics

- **Linux**: Uses CLI commands like `apt-get`. Requires a Debian-based system.
- **macOS**: Uses Homebrew (`brew`) via the implementation in `daemon/src/package_manager/brew.rs`.
- **Conditional Compilation**: Extensive use of `#[cfg(target_os = "linux")]` and `#[cfg(target_os = "macos")]` to handle package manager differences.
