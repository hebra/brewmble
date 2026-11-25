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

   # Trigger an upgrade and print its job ID
    ./cli/target/release/brewmble node --full-upgrade <node>
    # ("packages" is an alias for "node")

    # Wait for the upgrade to finish (exits nonzero if it fails)
    ./cli/target/release/brewmble node --full-upgrade --wait <node>

    # Preview an upgrade without executing it
    ./cli/target/release/brewmble node --full-upgrade --dry-run <node>

    # Reboot a node (only if the daemon has reboot enabled)
    ./cli/target/release/brewmble node --reboot <node>

    # Show CLI and daemon versions
    ./cli/target/release/brewmble version
    ```

Without `--wait`, the CLI returns once the daemon accepts the upgrade. Use the
job ID to check `running`, `succeeded`, or `failed` (with an error) via
[`GET /packages/full-upgrade/{job_id}`](daemon/README.md#get-packagesfull-upgradejob_id).
With `--wait`, the CLI checks each unfinished job in turn. It polls for up to
one hour after submitting upgrades, or for the duration set by
`--wait-timeout` (for example, `--wait-timeout 30m` or `--wait-timeout 1800`).
If time runs out, unfinished jobs are reported as unknown and the CLI exits
nonzero; it does not cancel running upgrades. `--wait` also exits nonzero if
there are no target nodes. Dry runs do not create jobs and cannot be combined
with `--wait`.

## Security

Brewmble uses mutual TLS (mTLS) for all CLI-to-daemon communication. A single cluster CA, managed by the CLI, signs both daemon and client certificates.

### 1. Mutual TLS (Built-in)

- **CA management**: `brewmble ca init` creates a self-signed ECDSA P-256 cluster CA in the OS keyring.
- **Daemon certificates**: `brewmble cert issue --kind daemon --cn <hostname> --out-dir <dir>` produces `ca.pem`, `daemon.pem`, and `daemon-key.pem`.
- **Client certificate**: `brewmble cert issue --kind client` stores the CLI client certificate in the keyring. Add `--role read` for a certificate limited to `/health` and `/status`, and `--out-dir <dir>` to write the certificate to files (for another host or a monitoring tool) instead of replacing the CLI's own.
- **Roles**: Each client certificate carries a role in its subject (`OU=brewmble-role:read` or `OU=brewmble-role:operator`, the default). `read` may call `GET /health` and `GET /status`; `operator` may also trigger and track upgrades and manage reboots. Certificates without a role, such as those issued by older CLI versions, are treated as `read`; re-issue them with `brewmble cert issue --kind client --force`.
- **Allow-list and revocation**: List the SHA-256 fingerprints of permitted client certificates under `tls.allowed_clients` in `brewmbled.yaml`. The daemon rejects any other certificate with `403 Forbidden`, even one signed by the cluster CA. To revoke a certificate, remove its fingerprint and reload the daemon (`systemctl reload brewmbled`, or send it `SIGHUP`); the change also applies to open connections. If `tls.allowed_clients` is not set, every certificate signed by the cluster CA is accepted, and `brewmbled config validate` warns about it.
- **Verification**: The daemon accepts client certificates issued by the cluster CA, subject to the allow-list and role checks above. The CLI checks the daemon's certificate against the cluster CA, verifies that it matches the target hostname or IP address, and checks the leaf fingerprint on the connection carrying the request. A pin failure prevents requests to that target. Use the DNS name in the daemon certificate, or use an IP address only if the certificate includes that IP as a subject alternative name.

#### Certificate Lifecycle and Operational Notes

- **Validity**: Cluster CA certificates are valid for ~10 years; daemon and client leaf certificates are valid for 397 days. Plan renewal before expiry.
- **Key Usage**: Daemon certificates include `ServerAuth` EKU and a DNS SAN matching the provided `--cn`. Client certificates include `ClientAuth` EKU, a DNS SAN matching the local hostname (or `--cn`), and their role as a subject OU.
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

The daemon normally runs as an unprivileged user. Linux package management and the optional reboot feature require elevated privileges through `sudo`, using well-known command paths.

- **Linux package management**: The daemon uses `apt-get` for updates, upgrades, autoclean, and autoremove. Configure the exact commands as shown in the [daemon README](daemon/README.md#sudo-configuration); sudoers matches both the resolved binary path and its arguments.
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
  - Restrict paths with `ProtectSystem=full` and `ProtectHome=read-only`, as in the sample unit.
  - Keep `NoNewPrivileges=false` and leave `CapabilityBoundingSet` unrestricted. Setting `NoNewPrivileges=true` or an empty `CapabilityBoundingSet=` prevents `sudo` from elevating privileges; on distributions using sudo-rs, the latter can also produce a `could not restore to saved user id` panic.
  - Use `ProtectKernelTunables=true`.

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
