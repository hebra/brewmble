# Brewmble CLI

The Brewmble CLI (`brewmble`) is a command-line interface for managing Brewmble daemons across your network. It uses mDNS for automatic discovery and interacts with daemons via their REST API.

## Table of Contents

- [Installation](#installation)
- [Usage](#usage)
  - [Discovery](#discovery)
  - [Status](#status)
  - [Package Management](#package-management)
  - [Profile Management](#profile-management)
- [Security](#security)
- [Configuration](#configuration)
  - [Configuration File](#configuration-file)
  - [Environment Variables](#environment-variables)
- [Development](#development)

## Installation

To build the CLI:

```bash
cargo build --release
```

The binary will be located at `target/release/brewmble`.

## Usage

### Discovery

Discover all Brewmble daemons on the local network:

```bash
brewmble discover [--timeout <seconds>] [--update-config]
```

Use `--update-config` (or `-u`) to save discovered daemons to your configuration file.

### Status

Check the status of one or more daemons:

```bash
# Check all daemons from the configuration file
brewmble status

# Check all discovered daemons
brewmble status --all

# Check specific daemons
brewmble status <host:port> [<host:port> ...]
```

### Package Management

Trigger a full system upgrade on target nodes:

```bash
# Upgrade all nodes from the configuration file
brewmble packages --full-upgrade

# Upgrade specific target nodes
brewmble packages --full-upgrade <target> [<target> ...]
```

### Certificate Management

Initialize the cluster CA and issue certificates for daemons and this CLI:

```bash
# Create the cluster CA in the system keyring
brewmble ca init

# Show the CA certificate and fingerprint
brewmble ca show

# Issue a daemon certificate (run on a workstation, then provision the PEMs)
brewmble cert issue --kind daemon --cn <hostname> --out-dir <dir>

# Issue this CLI's client certificate and store it in the keyring
brewmble cert issue --kind client
```

### Profile Management

Manage configuration profiles:

```bash
# List all profiles
brewmble profile list

# Create a new profile
brewmble profile create <name>

# Switch to a profile
brewmble profile use <name>
```

## Security

Brewmble CLI communicates with daemons exclusively over HTTPS using mutual TLS (mTLS). The CLI stores the cluster CA and its own client certificate in the OS keyring. Daemon hostnames are not verified; only the cluster CA chain is checked. See the [main Security section](../README.md#security) for details.

## Configuration

The CLI can be configured via a YAML configuration file (`.brewmble.yaml`) and environment variables.

### Configuration File

The CLI searches for a configuration file in the following order:
1.  Path specified via the `--config` (or `-c`) flag.
2.  Path specified via the `BREWMBLE_CONFIG` environment variable.
3.  The current working directory (`./.brewmble.yaml`).

#### Structure

```yaml
active_profile: default
profiles:
  default:
    nodes:
      - name: production-1
        address: 192.168.1.10:4712
      - name: raspberry-pi
        address: 192.168.1.50:4712
```

The `api_key` and `use_keyring` fields from older versions are ignored; authentication is now performed with mTLS client certificates managed via `brewmble cert`.

### Environment Variables

- `BREWMBLE_TIMEOUT`: Default timeout for network operations (e.g., `30s`, `1m`). Default is `60s`.
- `BREWMBLE_CONFIG`: Path to the configuration file.

## Development

### Running Tests

```bash
cargo test
```
