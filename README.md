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

## Development

See the individual component directories for specific development instructions:
- [CLI Development](./cli/README.md)
- [Daemon Development](./daemon/README.md)

## License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
