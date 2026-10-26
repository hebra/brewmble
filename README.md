# Brewmble

Brewmble is a powerful and flexible management tool for Linux and macOS systems.
It centralises and automates the process of keeping systems up-to-date.
The main use case for Brewmble is in small Raspberry Pi clusters or home labs,
where it simplifies the maintenance of multiple devices.

## Table of Contents

- [Features](#features)
- [Components](#components)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Installation](#installation)
- [Usage](#usage)
- [Security](#security)
  - [1. Mutual TLS (Built-in)](#1-mutual-tls-built-in)
  - [2. SSH Tunneling](#2-ssh-tunneling)
  - [3. Private Overlay Networks](#3-private-overlay-networks)
- [Configuration](#configuration)
- [Development](#development)
- [License](#license)

## Features

- **Automated Updates**: Centralised management for system updates across multiple nodes.
- **Multi-backend Support**: Supports different package managers (currently APT and Homebrew).
- **mDNS Discovery**: Automatic discovery of Brewmble daemons on the local network.
- **RESTful API**: Each node provides a REST API for status and management.
- **CLI Tool**: A unified command-line interface to manage your entire cluster.
- **Containerized**: Ready to run as a container for easy deployment.

## Components

Brewmble consists of several key components:

- **[Brewmble Daemon](./daemon)**: A background service (`brewmbled`) that runs on each managed node. It interacts with the local package manager (APT or Homebrew) and exposes a REST API.
- **[Brewmble CLI](./cli)**: A command-line tool (`brewmble`) for humans to interact with one or more daemons.
- **[Brewmble REST](./rest)**: A shared Rust crate defining the API data models, endpoint paths, and service-discovery constants used by both the CLI and the daemon.
- **[Brewmble Web](./web)**: (Planned, not yet implemented) A web-based dashboard for cluster overview.

## Getting Started

### Prerequisites

- Rust (latest stable)
- Linux (APT) or macOS (Homebrew) system
- mDNS/Avahi support (for discovery)

### Installation

#### Install Rust and Cargo
```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.bashrc
```

#### Using cargo

```shell
# CLI tools
cargo install --git https://codeberg.org/heb/brewmble.git brewmble

# Daemon
cargo install --git https://codeberg.org/heb/brewmble.git brewmbled
```

#### From cloned sources

To build all components:

```bash
# Build CLI
cd cli && cargo build --release

# Build Daemon
cd daemon && cargo build --release
```

## Usage

1. Start the daemon on your target nodes:
   ```bash
   ./daemon/target/release/brewmbled
   ```

2. Use the CLI to discover and manage nodes:
   ```bash
   # Discover nodes
   ./cli/target/release/brewmble discover

   # Check status
   ./cli/target/release/brewmble status --all

   # List configured nodes in active profile
   ./cli/target/release/brewmble node --list

   # Trigger upgrade
    ./cli/target/release/brewmble node --full-upgrade <node>
    # ("packages" is an alias for "node")

    # Preview an upgrade without executing it
    ./cli/target/release/brewmble node --full-upgrade --dry-run <node>

    # Reboot a node (only if the daemon has reboot enabled)
    ./cli/target/release/brewmble node --reboot <node>

    # Show CLI and daemon versions
    ./cli/target/release/brewmble version
    ```

## Security

Brewmble uses mutual TLS (mTLS) for all CLI-to-daemon communication. A single cluster CA, managed by the CLI, signs both daemon and client certificates.

### 1. Mutual TLS (Built-in)

- **CA management**: `brewmble ca init` creates a self-signed ECDSA P-256 cluster CA in the OS keyring.
- **Daemon certificates**: `brewmble cert issue --kind daemon --cn <hostname> --out-dir <dir>` produces `ca.pem`, `daemon.pem`, and `daemon-key.pem`.
- **Client certificate**: `brewmble cert issue --kind client` stores the CLI client certificate in the keyring.
- **Verification**: The daemon accepts any client certificate signed by the cluster CA. Hostname verification is disabled so DHCP/IP churn does not break connectivity.

#### Certificate Lifecycle and Operational Notes

- **Validity**: Cluster CA certificates are valid for ~10 years; daemon and client leaf certificates are valid for 397 days. Plan renewal before expiry.
- **Key Usage**: Daemon certificates include `ServerAuth` EKU and a DNS SAN matching the provided `--cn`. Client certificates include `ClientAuth` EKU and a DNS SAN matching the local hostname.
- **Private keys**: Daemon private keys are written with `0600` permissions. Keep the OS keyring (where the CLI stores CA and client keys) backed up and protected.
- **Validation**: Run `brewmbled config validate` after rotating daemon certificates. It checks file presence, certificate/key pair-match, expiry (errors if expired, warns if < 30 days), and required EKU.
- **Rotation workflow**:
  ```bash
  # On the admin workstation
  brewmble cert issue --kind daemon --cn node1 --out-dir ./node1-certs --force
  # Copy ca.pem, daemon.pem, and daemon-key.pem to the node and update brewmbled.yaml tls: paths
  brewmbled config validate
  brewmbled config reload   # if running via systemd
  ```
- **Fingerprints**: The CLI logs the SHA-256 fingerprint of every issued certificate. Use `brewmble ca show` to display the CA fingerprint.
- **Keyring store migration**: After upgrading the CLI, run `brewmble ca migrate` to verify that CA and client credentials are accessible. On macOS, the CLI uses the modern `SecItem*` keychain API; entries created by older CLI versions (via the legacy `SecKeychain*` API) are detected and re-stored automatically. Use `--dry-run` to preview changes.

### Privilege Model and Service Hardening

The daemon normally runs as an unprivileged user. Only the reboot feature requires elevated privileges, and it is invoked through well-known absolute paths.

- **Reboot commands**: When `features.reboot` is enabled, the daemon uses:
  - `/usr/bin/sudo -n /usr/bin/systemctl reboot`
  - fallback: `/usr/bin/sudo -n /usr/sbin/reboot`
- **Recommended `sudoers` snippet** (use `visudo`):
  ```sudoers
  brewmble ALL=(root) NOPASSWD: /usr/bin/systemctl reboot, /usr/sbin/reboot
  ```
  Replace `brewmble` with the user the daemon runs as. Never grant a blanket `NOPASSWD: ALL` rule.
- **Systemd unit hardening** (recommended):
  - Run the daemon as a dedicated user (`User=brewmble`).
  - Set `Restart=on-failure` and `RestartSec=5`.
  - Restrict paths with `ProtectSystem=strict`, `ProtectHome=true`, and `ReadWritePaths=/var/lib/brewmble` if needed.
  - Drop capabilities with `CapabilityBoundingSet=` and allow only `CAP_SYS_BOOT` if reboot is enabled.
  - Use `NoNewPrivileges=true` and `ProtectKernelTunables=true`.

### 2. SSH Tunneling

A simple way to secure communication without additional infrastructure:
- **Setup**: Create a tunnel: `ssh -L 4712:localhost:4712 user@remote-node`
- **CLI**: Connect to `localhost:4712`.

### 3. Private Overlay Networks

For clusters, using a private network layer is recommended:
- **Tools**: Use Tailscale or ZeroTier to create an encrypted mesh network.
- **Benefits**: Provides end-to-end encryption and isolates the daemon from the public local network.

## Configuration

Brewmble components are configured via declarative YAML files and CLI subcommands:

### Daemon Configuration (`brewmbled.yaml`)

The daemon loads configuration from `~/.config/brewmble/brewmbled.yaml`, `/etc/brewmble/brewmbled.yaml`, or `./brewmbled.yaml` (or via `--config <path>` / `-c <path>`).

Manage daemon configuration with:

```bash
brewmbled config init       # Generate initial template
brewmbled config show       # Display active configuration
brewmbled config edit       # Open configuration in $EDITOR
brewmbled config validate   # Pre-flight check certificates and settings
```

See the [Daemon README](./daemon/README.md#configuration) for the complete YAML schema.

### CLI Configuration (`.brewmble.yaml`)

The CLI uses profile-based configuration stored in `~/.brewmble.yaml` or `.brewmble.yaml`. Manage profiles with `brewmble profile (list|use|create|delete|edit)`.

### Environment Variables

| Variable | Component | Description | Default |
|:---|:---|:---|:---|
| `BREWMBLE_CONFIG` | CLI | Path to the CLI configuration file. | `~/.brewmble.yaml` / `.brewmble.yaml` |
| `BREWMBLE_TIMEOUT` | CLI | Timeout for HTTP requests (seconds or [humantime](https://docs.rs/humantime), e.g., `1m`, `30s`) and default `discover` scan duration (seconds). | `5s` (discovery), `60s` (HTTP) |
| `BREWMBLE_NATIVE_PROGRESS` | CLI | Native terminal progress bars (unset or 1 to enable, 0 to disable). | enabled |
| `RUST_LOG` | Daemon | Logging filter (e.g., `info`, `debug`). | `brewmbled=info,tower_http=debug,axum::rejection=trace` |
| `CONTAINER_TOOL` | Makefile | Tool used for container operations. | `podman` |

## Development

See the individual component directories for specific development instructions:
- [Project Brief](./PROJECT_BRIEF.md)
- [Architecture](./ARCHITECTURE.md)
- [Roadmap](./plans/roadmap.md)
- [CLI Development](./cli/README.md)
- [Daemon Development](./daemon/README.md)

### CI/CD and Local Testing

The GitHub Actions workflows can be tested locally using [act](https://github.com/nektos/act). 
A `.actrc` file is provided to map `macos-latest` to a compatible Docker image.

To run the daemon build workflow locally:
```bash
act workflow_dispatch
```

### Container Notes

The daemon container runs as an unprivileged `brewmble` user. It is intended for brew-less or host-package-manager scenarios; if you need the daemon to perform `apt` operations inside the container, run it as root and supply the required capabilities. mDNS requires `--network host` on Linux or an mDNS reflector; the Makefile target uses host networking and exposes UDP port 5353.

## License

This project is licensed under the GNU General Public License v3.0 (GPL-3.0). See the [LICENSE](LICENSE) file for details.
