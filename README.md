# Cobbler

Cobbler is a powerful and flexible management tool for Linux and macOS systems.
It centralises and automates the process of keeping systems up-to-date.
The main use case for Cobbler is in small Raspberry Pi clusters or home labs,
where it simplifies the maintenance of multiple devices.

## Features

- **Automated Updates**: Centralised management for system updates across multiple nodes.
- **Multi-backend Support**: Supports different package managers (currently APT and Homebrew).
- **mDNS Discovery**: Automatic discovery of Cobbler daemons on the local network.
- **RESTful API**: Each node provides a REST API for status and management.
- **CLI Tool**: A unified command-line interface to manage your entire cluster.
- **Containerized**: Ready to run as a container for easy deployment.

## Components

Cobbler consists of several key components:

- **[Cobbler Daemon](./daemon)**: A background service (`cobblerd`) that runs on each managed node. It interacts with the local package manager (APT or Homebrew) and exposes a REST API.
- **[Cobbler CLI](./cli)**: A command-line tool (`cobbler`) for humans to interact with one or more daemons.
- **Cobbler REST**: The REST API specification used for communication between components.
- **Cobbler Web**: (In development) A web-based dashboard for cluster overview.

## Getting Started

### Prerequisites

- Rust (latest stable)
- Linux (APT) or macOS (Homebrew) system
- mDNS/Avahi support (for discovery)

### Installation

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
   ./daemon/target/release/cobblerd
   ```

2. Use the CLI to discover and manage nodes:
   ```bash
   # Discover nodes
   ./cli/target/release/cobbler discover

   # Check status
   ./cli/target/release/cobbler status --all

   # Trigger upgrade
   ./cli/target/release/cobbler packages --full-upgrade <target>
   ```

## Configuration

Cobbler can be configured using environment variables.

### Environment Variables

| Variable | Component | Description | Default |
|:---|:---|:---|:---|
| `COBBLER_DAEMON_PORT` | Daemon | Port for the daemon to listen on. If not specified, the daemon will search for a free port starting from 8080. | `8080` (auto-hunt) |
| `COBBLER_DAEMON_HOSTNAME` | Daemon | Hostname to use for mDNS registration. | System hostname |
| `COBBLER_DAEMON_IP` | Daemon | Explicit IP address to use for mDNS registration. | Automatically detected |
| `COBBLER_DAEMON_API_KEY` | Daemon | API key for authentication. If not provided, a random UUID v4 will be generated and logged. | Generated |
| `COBBLER_TIMEOUT` | CLI | Timeout for discovery and HTTP requests. Supports seconds or [humantime](https://docs.rs/humantime) (e.g., `1m`, `30s`). | `5s` (discovery), `60s` (HTTP) |
| `COBBLER_CONFIG` | CLI | Path to the CLI configuration file. | `.cobbler.yaml` |
| `RUST_LOG` | Daemon | Logging filter for the daemon (e.g., `info`, `debug`). | `cobblerd=info` |
| `CONTAINER_TOOL` | Makefile | Tool used for container operations. | `podman` |

## Development

See the individual component directories for specific development instructions:
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

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
