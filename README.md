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
  - [1. API Key Authentication (Built-in)](#1-api-key-authentication-built-in)
  - [2. HTTPS via Reverse Proxy](#2-https-via-reverse-proxy)
  - [3. SSH Tunneling](#3-ssh-tunneling)
  - [4. Private Overlay Networks](#4-private-overlay-networks)
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
- **Brewmble REST**: The REST API specification used for communication between components.
- **Brewmble Web**: (In development) A web-based dashboard for cluster overview.

## Getting Started

### Prerequisites

- Rust (latest stable)
- Linux (APT) or macOS (Homebrew) system
- mDNS/Avahi support (for discovery)

### Installation

#### Using cargo


#### Create a local daemon user (Linux)
```shell
sudo useradd -md /opt/brewmble brewmble
sudo su brewmble
```

*Note: For Linux systems, you must also configure `sudo` to allow the `brewmble` user to run `apt` commands. See the [Sudo Configuration section in the Daemon README](./daemon/README.md#sudo-configuration) for details.*

#### Install Rust and Cargo
```shell
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.bashrc
```



```shell
# CLI tools
cargo install --git https://github.com/hebra/brewmble.git brewmble

# Daemon
cargo install --git https://github.com/hebra/brewmble.git brewmbled
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

   # Trigger upgrade
   ./cli/target/release/brewmble packages --full-upgrade <target>
   ```

## Security

Brewmble provides several options to secure communication between the CLI and daemons:

### 1. API Key Authentication (Built-in)

The primary security layer is API Key authentication.
- **Daemon**: Set `BREWMBLE_DAEMON_API_KEY`. If not provided, a random UUID v4 is generated and logged at startup.
- **CLI**: Store keys in `.brewmble.yaml` for each node, or use the system keyring for better security.
- **Protocol**: All requests must include the `X-API-Key` header.

### 2. HTTPS via Reverse Proxy

For encrypted traffic over the network, you can use HTTPS:
- **Setup**: Place a reverse proxy (e.g., Caddy, Nginx, or Traefik) in front of the daemon to handle TLS termination.
- **CLI**: Use `https://` in the node address (e.g., `https://node1.example.com`).

### 3. SSH Tunneling

A simple way to secure communication without additional infrastructure:
- **Setup**: Create a tunnel: `ssh -L 8080:localhost:8080 user@remote-node`
- **CLI**: Connect to `localhost:8080`.

### 4. Private Overlay Networks

For clusters, using a private network layer is recommended:
- **Tools**: Use Tailscale or ZeroTier to create an encrypted mesh network.
- **Benefits**: Provides end-to-end encryption and isolates the daemon from the public local network.

## Configuration

Brewmble can be configured using environment variables.

### Environment Variables

| Variable | Component | Description | Default |
|:---|:---|:---|:---|
| `BREWMBLE_DAEMON_PORT` | Daemon | Port for the daemon to listen on. If not specified, the daemon will search for a free port starting from 8080. | `8080` (auto-hunt) |
| `BREWMBLE_DAEMON_HOSTNAME` | Daemon | Hostname to use for mDNS registration. | System hostname |
| `BREWMBLE_DAEMON_IP` | Daemon | Explicit IP address to use for mDNS registration. | Automatically detected |
| `BREWMBLE_DAEMON_API_KEY` | Daemon | API key for authentication. If not provided, a random UUID v4 will be generated and logged. | Generated |
| `BREWMBLE_APT_UPDATE_INTERVAL` | Daemon | Interval in minutes between `apt-get update` calls. Set to `0` to always update. | `360` (6 hours) |
| `BREWMBLE_TIMEOUT` | CLI | Timeout for discovery and HTTP requests. Supports seconds or [humantime](https://docs.rs/humantime) (e.g., `1m`, `30s`). | `5s` (discovery), `60s` (HTTP) |
| `BREWMBLE_CONFIG` | CLI | Path to the CLI configuration file. | `.brewmble.yaml` |
| `RUST_LOG` | Daemon | Logging filter for the daemon (e.g., `info`, `debug`). | `brewmbled=info` |
| `CONTAINER_TOOL` | Makefile | Tool used for container operations. | `podman` |

## Development

See the individual component directories for specific development instructions:
- [Project Brief](./PROJECT_BRIEF.md)
- [Architecture](./ARCHITECTURE.md)
- [Roadmap](./ROADMAP.md)
- [CLI Development](./cli/README.md)
- [Daemon Development](./daemon/README.md)

### CI/CD and Local Testing

The GitHub Actions workflows can be tested locally using [act](https://github.com/nektos/act). 
A `.actrc` file is provided to map `macos-latest` to a compatible Docker image.

To run the daemon build workflow locally:
```bash
act workflow_dispatch
```

## License

This project is licensed under the GNU General Public License v3.0 (GPL-3.0). See the [LICENSE](LICENSE) file for details.
