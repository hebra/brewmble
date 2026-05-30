# Brewmble CLI

The Brewmble CLI (`brewmble`) is a command-line interface for managing Brewmble daemons across your network. It uses mDNS for automatic discovery and interacts with daemons via their REST API.

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

### Profile Management

Manage configuration profiles and secure API keys:

```bash
# List all profiles
brewmble profile list

# Create a new profile
brewmble profile create <name>

# Switch to a profile
brewmble profile use <name>

# Securely store an API key for a node in the system keyring
brewmble profile set-key <node-address-or-name> <api-key>
```

## Security

Brewmble CLI supports several secure communication methods, including API keys, HTTPS, and SSH tunneling. See the [main Security section](../README.md#security) for details.

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
        address: 192.168.1.10:8080
        api_key: your-secret-api-key
      - name: raspberry-pi
        address: 192.168.1.50:8080
        use_keyring: true # API key is stored in system keyring
```

### Environment Variables

- `BREWMBLE_TIMEOUT`: Default timeout for network operations (e.g., `30s`, `1m`). Default is `60s`.
- `BREWMBLE_CONFIG`: Path to the configuration file.

## Development

### Running Tests

```bash
cargo test
```
