# Brewmble CLI

The Brewmble CLI (`brewmble`) is a command-line interface for managing Brewmble daemons across your network. It uses mDNS for automatic discovery and interacts with daemons via their REST API.

## Table of Contents

- [Installation](#installation)
- [Usage](#usage)
  - [Discovery](#discovery)
  - [Status](#status)
  - [Node Management](#node-management)
  - [Certificate Management](#certificate-management)
  - [Profile Management](#profile-management)
  - [Version Information](#version-information)
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

The scan waits up to `--timeout` (or `-t`) seconds for responses (default: `5`). Use `--update-config` (or `-u`) to save discovered daemons to your configuration file.

### Status

Check the status of one or more daemons:

```bash
# Check all daemons from the configuration file
brewmble status

# Check all discovered daemons
brewmble status --all

# Check specific daemons (host:port, or a name from the active profile)
brewmble status <target> [<target> ...]
```

Targets are resolved against the active profile by name or address, and the output ends with a summary of outdated and unreachable nodes.

### Node Management

Trigger a full system upgrade, reboot target nodes, or list nodes configured in the active profile. The `node` command is also available under the alias `packages`:

```bash
# List all nodes configured in the active profile
brewmble node --list
# or: brewmble node -l

# Upgrade all nodes from the active profile
brewmble node --full-upgrade
# or: brewmble packages -u

# Preview what would be upgraded without executing
brewmble node --full-upgrade --dry-run <node>

# Upgrade specific nodes by name
brewmble node --full-upgrade <node> [<node> ...]

# Reboot a node (only if the daemon has allow_reboot enabled)
brewmble node --reboot <node>
# or: brewmble node -r <node>
```

### Certificate Management

Initialize the cluster CA and issue certificates for daemons and this CLI:

```bash
# Create the cluster CA in the system keyring (use --force to overwrite an existing CA)
brewmble ca init

# Show the CA certificate and fingerprint
brewmble ca show

# Issue a daemon certificate (run on a workstation, then provision the PEMs)
brewmble cert issue --kind daemon --cn <hostname> --out-dir <dir>

# Issue this CLI's client certificate and store it in the keyring
brewmble cert issue --kind client
# Re-issue an existing client certificate
brewmble cert issue --kind client --force
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

# Delete a profile (cannot delete the default or active profile)
brewmble profile delete <name>

# Open the configuration file in $EDITOR
brewmble profile edit
```

### Version Information

Show the CLI version together with the daemon versions of the nodes in the active profile:

```bash
brewmble version
```

## Security

Brewmble CLI communicates with daemons exclusively over HTTPS using mutual TLS (mTLS). The CLI stores the cluster CA and its own client certificate in the OS keyring. Daemon hostnames are not verified; only the cluster CA chain is checked. See the [main Security section](../README.md#security) for details.

## Configuration

The CLI can be configured via a YAML configuration file (`.brewmble.yaml`) and environment variables.

### Configuration File

The CLI searches for a configuration file in the following order:
1.  Path specified via the `--config` (or `-c`) flag.
2.  Path specified via the `BREWMBLE_CONFIG` environment variable.
3.  The home directory (`~/.brewmble.yaml`).
4.  The current working directory (`./.brewmble.yaml`).

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

A legacy top-level `nodes:` list from older versions is automatically migrated into the `default` profile when the file is loaded. Per-node `api_key` fields from older versions are silently ignored; authentication is now performed with mTLS client certificates managed via `brewmble cert`.

### Environment Variables

- `BREWMBLE_TIMEOUT`: Timeout for HTTP requests (full seconds or [humantime](https://docs.rs/humantime) values such as `1m` or `30s`; default `60s`) and the default discovery scan duration (whole seconds; default `5s`).
- `BREWMBLE_CONFIG`: Path to the configuration file.
- `BREWMBLE_NATIVE_PROGRESS`: Emit native terminal progress sequences (`OSC 9;4`) during `status`. Unset or `1` enables them (the default); set to `0` to disable and fall back to in-terminal progress bars.
- `EDITOR`: Editor used by `brewmble profile edit` (default: `vi`).

## Development

### Running Tests

```bash
cargo test
```
