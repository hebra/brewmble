# Cobbler CLI

The Cobbler CLI (`cobbler`) is a command-line interface for managing Cobbler daemons across your network. It uses mDNS for automatic discovery and interacts with daemons via their REST API.

## Installation

To build the CLI:

```bash
cargo build --release
```

The binary will be located at `target/release/cobbler`.

## Usage

### Discovery

Discover all Cobbler daemons on the local network:

```bash
cobbler discover [--timeout <seconds>] [--update-config]
```

Use `--update-config` (or `-u`) to save discovered daemons to your configuration file.

### Status

Check the status of one or more daemons:

```bash
# Check all daemons from the configuration file
cobbler status

# Check all discovered daemons
cobbler status --all

# Check specific daemons
cobbler status <host:port> [<host:port> ...]
```

### Package Management

Trigger a full system upgrade on target nodes:

```bash
# Upgrade all nodes from the configuration file
cobbler packages --full-upgrade

# Upgrade specific target nodes
cobbler packages --full-upgrade <target> [<target> ...]
```

## Security

Cobbler CLI supports several secure communication methods, including API keys, HTTPS, and SSH tunneling. See the [main Security section](../README.md#security) for details.

## Configuration

The CLI can be configured via a YAML configuration file (`.cobbler.yaml`) and environment variables.

### Configuration File

The CLI searches for a configuration file in the following order:
1.  Path specified via the `--config` (or `-c`) flag.
2.  Path specified via the `COBBLER_CONFIG` environment variable.
3.  The current working directory (`./.cobbler.yaml`).

#### Structure

```yaml
nodes:
  - name: production-1
    address: 192.168.1.10:8080
    api_key: your-secret-api-key
  - address: 192.168.1.11:8080
```

### Environment Variables

- `COBBLER_TIMEOUT`: Default timeout for network operations (e.g., `30s`, `1m`). Default is `60s`.
- `COBBLER_CONFIG`: Path to the configuration file.

## Development

### Running Tests

```bash
cargo test
```
