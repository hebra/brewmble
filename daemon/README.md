# Brewmble Daemon

The Brewmble Daemon (`brewmbled`) is a background service that runs on managed nodes. It provides a REST API for system status and package management, supporting multiple backends like APT (Linux) and Homebrew (macOS).

## Table of Contents

- [Features](#features)
- [Installation](#installation)
  - [Creating a Dedicated User (Linux)](#creating-a-dedicated-user-linux)
  - [Install Rust and Cargo](#install-rust-and-cargo)
  - Install brewmbled daemon
    - [From Remote Codeberg.org Repository](#from-remote-codebergorg-repository)
    - [From Local Source](#from-local-source)
  - [Initial Configuration](#initial-configuration)
    - [Create Configuration File](#create-configuration-file) 
    - [TLS Setup](#tls-setup)
    - [Running as a systemd service (Linux)](#running-as-a-systemd-service-linux)
    - [Running as a Launch Agent (macOS)](#running-as-a-launch-agent-macos)
    - [Sudo Configuration](#sudo-configuration)
- [Configuration](#configuration)
- [Security and Authentication](#security-and-authentication)
- [API Endpoints](#api-endpoints)
  - [`GET /health`](#get-health)
  - [`GET /status`](#get-status)
  - [`POST /packages/full-upgrade`](#post-packagesfull-upgrade)
  - [`POST /node/reboot`](#post-nodereboot)
- [Development](#development)
  - [Running Tests](#running-tests)

## Features

- **mDNS Registration**: Automatically announces itself on the local network as `_brewmble._tcp.local.` with the instance name `brewmbled-<hostname>` and TXT properties `id=<hostname>` and `tls=mtls`. Publishes A/AAAA records under a dedicated `brewmbled-<hostname>.local` host label to avoid colliding with the system mDNS responder. Can be disabled with `server.mdns: false`.
- **Multi-backend Support**: Automatically detects and uses the available package manager (APT or Homebrew).
- **System Status**: Reports whether the system is up-to-date and lists available updates.
- **Package Management**: Can trigger a full system upgrade via the detected package manager.
- **Automatic Cleanup**: Optional `apt-get autoclean` / `brew cleanup` and `apt-get autoremove -y` / `brew autoremove` after successful upgrades.
- **Controlled Reboots**: Optional API-triggered system reboot, gated by a daemon configuration flag.
- **Authentication**: Secure access via mutual TLS (mTLS). The daemon requires a valid client certificate signed by the cluster CA.
- **Port Hunting**: Automatically finds an available port starting from 4712 if not specified.

## Installation

### Creating a Dedicated User (Linux)

To run the daemon securely as a dedicated service account on Linux:

```bash
sudo useradd -md /opt/brewmble -s /bin/bash brewmble
sudo su brewmble
```

*Note: For Linux systems, you must also configure `sudo` to allow the `brewmble` user to run `apt` commands. See the [Sudo Configuration](#sudo-configuration) section for details.*

### Install basic build tools (Linux)

```bash
sudo apt update
sudo apt install build-essential
```

### Install Rust and Cargo

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.bashrc
```

### Install brewmbled daemon

#### From Remote Codeberg.org Repository

```bash
cargo install --git https://codeberg.org/heb/brewmble.git brewmbled
```

#### From Local Source

```bash
cargo build --release
```

The binary will be located at `target/release/brewmbled`.

### Initial Configuration

#### Create Configuration File

The brewmbled daemon uses a YAML file for environment-specific configuration.
To initialise the configuration with default values, run:

```bash
brewmbled config init
```
To edit the configuration using the default editor, run:

```bash
brewmbled config edit
```


#### TLS Setup

The daemon requires three PEM files from the cluster CA:

1. On a machine with the `brewmble` CLI installed, initialize the cluster CA:
   ```bash
   brewmble ca init
   ```
2. Issue a daemon certificate for the target host:
   ```bash
   brewmble cert issue --kind daemon --cn <hostname> --out-dir ./daemon-certs
   ```
3. Copy the three PEM files to the daemon host:
   ```bash
   scp ./daemon-certs/* root@<hostname>:/etc/brewmble/tls/
   ```
4. Initialize and configure the daemon configuration file with `brewmbled config init` and `brewmbled config edit`, specifying the certificate paths under the `tls:` section.

#### Running as a systemd service (Linux)

For Linux systems, a sample systemd service file is provided in the `docs` folder.

##### Setup

1. **Copy the service file**:
   ```bash
   sudo cp ../docs/brewmbled.service.sample /etc/systemd/system/brewmbled.service
   ```

2. **Configure the service**:
   Open the file in an editor to adjust the `ExecStart` path. All daemon settings (including TLS certificate paths) come from the YAML configuration file passed via `--config`:
   ```bash
   sudo nano /etc/systemd/system/brewmbled.service
   ```

3. **Reload systemd**:
   ```bash
   sudo systemctl daemon-reload
   ```

##### Managing the Service

- **Start the daemon**:
  ```bash
  sudo systemctl start brewmbled
  ```

- **Stop the daemon**:
  ```bash
  sudo systemctl stop brewmbled
  ```

- **Enable auto-start on boot**:
  ```bash
  sudo systemctl enable brewmbled
  ```

- **Disable auto-start on boot**:
  ```bash
  sudo systemctl disable brewmbled
  ```

- **Check status**:
  ```bash
  sudo systemctl status brewmbled
  ```

- **View logs**:
  ```bash
  sudo journalctl -u brewmbled
  ```

#### Running as a Launch Agent (macOS)

For macOS, you can run `brewmbled` as a background service for the logged-in user using `launchd`. A sample plist file is provided in the `docs` folder.

##### Setup

1. **Copy the plist file**:
   ```bash
   mkdir -p ~/Library/LaunchAgents
   cp ../docs/com.github.hebra.brewmble.brewmbled.plist.sample ~/Library/LaunchAgents/com.github.hebra.brewmble.brewmbled.plist
   ```

2. **Configure the service**:
   Open the file in an editor to adjust the `ProgramArguments` path and any environment variables:
   ```bash
   nano ~/Library/LaunchAgents/com.github.hebra.brewmble.brewmbled.plist
   ```

   > **Note**: You must update the path in `ProgramArguments` to point to your actual `brewmbled` binary (e.g., replace `USERNAME` in the sample path with your actual macOS username). You can also adjust the default values for the environment variables in the `EnvironmentVariables` section.

3. **Load the service**:
   ```bash
   launchctl load ~/Library/LaunchAgents/com.github.hebra.brewmble.brewmbled.plist
   ```

##### Managing the Service

- **Start the daemon**:
  ```bash
  launchctl start com.github.hebra.brewmble.brewmbled
  ```

- **Stop the daemon**:
  ```bash
  launchctl stop com.github.hebra.brewmble.brewmbled
  ```

- **Unload the service** (prevents auto-start):
  ```bash
  launchctl unload ~/Library/LaunchAgents/com.github.hebra.brewmble.brewmbled.plist
  ```

- **Check status**:
  ```bash
  launchctl list | grep brewmble
  ```

- **View logs**:
  ```bash
  tail -f /tmp/com.github.hebra.brewmble.brewmbled.out
  tail -f /tmp/com.github.hebra.brewmble.brewmbled.err
  ```

#### Sudo Configuration

The `brewmbled` daemon runs as the `brewmble` user but needs to perform package management operations that require root privileges. To enable this, you must configure `sudo` to allow the `brewmble` user to run `apt` commands without a password.

1. **Create a sudoers file**:
   It is recommended to create a separate file in `/etc/sudoers.d/`:
   ```bash
   sudo nano /etc/sudoers.d/brewmble
   ```

2. **Add the following content**:
   ```text
   brewmble ALL=(root) NOPASSWD: /usr/bin/apt, /usr/bin/apt-get, /usr/bin/systemctl reboot, /usr/sbin/reboot
   ```

   Adjust the paths to match your distribution (`/bin/systemctl`, `/sbin/reboot`, etc.).

3. **Set correct permissions**:
   The file must have strict permissions:
   ```bash
   sudo chmod 440 /etc/sudoers.d/brewmble
   ```

## Configuration

`brewmbled` is configured via a YAML configuration file.

### Configuration Discovery

The daemon searches for configuration files in the following hierarchical order:

1. **Explicit Path**: Specified via `--config <path>` or `-c <path>`.
2. **User Config**: `~/.config/brewmble/brewmbled.yaml`
3. **System Config**: `/etc/brewmble/brewmbled.yaml`
4. **Current Directory**: `./brewmbled.yaml`

If no configuration file is found and no explicit path is passed, daemon startup fails with a diagnostic error directing you to run `brewmbled config init`.

### Configuration Management Commands

`brewmbled config` provides dedicated subcommands for managing daemon configuration:

- **Initialize template configuration**:
  ```bash
  brewmbled config init
  # Overwrite existing file:
  brewmbled config init --force
  # Custom path:
  brewmbled -c /path/to/brewmbled.yaml config init
  ```
- **Inspect active configuration**:
  ```bash
  brewmbled config show
  ```
- **Edit configuration in `$EDITOR`**:
  ```bash
  brewmbled config edit
  ```
- **Pre-flight validation** (verifies YAML syntax and TLS certificate/key presence):
  ```bash
  brewmbled config validate
  ```

### Configuration Schema

```yaml
# Server networking and mDNS settings
server:
  # Port to listen on. If omitted, the daemon hunts for a free port starting at 4712.
  port: 4712

  # Hostname for mDNS registration (default: system hostname).
  hostname: "node-1"

  # Explicit IP address for mDNS registration (default: auto-detected).
  ip: "192.168.1.10"

  # Enable mDNS service discovery (default: true).
  mdns: true

# TLS / mutual TLS certificates (required)
tls:
  # Path to the cluster CA certificate PEM file
  ca_cert: "/etc/brewmble/tls/ca.pem"

  # Path to the daemon certificate PEM file
  cert: "/etc/brewmble/tls/daemon.pem"

  # Path to the daemon private key PEM file
  key: "/etc/brewmble/tls/daemon-key.pem"

# Daemon behavior flags
features:
  # Allow the daemon to reboot the host when requested via the API (default: false)
  allow_reboot: false

  # Automatically clean downloaded packages after upgrade (default: false)
  auto_clean: false

  # Automatically remove unused packages after upgrade (default: false)
  auto_remove: false

# Package manager backend caching
package_manager:
  # Cache update interval in minutes (default: 360, set to 0 to update on every status check)
  update_interval_mins: 360
```

## Security and Authentication

All API endpoints require a valid client certificate signed by the cluster CA. The daemon does not use API keys; TLS client-certificate verification is the sole authentication mechanism.

Provision the daemon with three PEM files:

- `ca.pem` — cluster CA certificate
- `daemon.pem` — daemon leaf certificate (CN = hostname, no SANs)
- `daemon-key.pem` — daemon private key (PKCS#8)

Configure their paths in your `brewmbled.yaml` under the `tls:` section, and verify permissions with:

```bash
brewmbled config validate
```

For more details, see the [main Security section](../README.md#security).

## API Endpoints

All endpoints are served over HTTPS and require a client certificate signed by the cluster CA.

### `GET /health`

Returns liveness information about the daemon and its detected package manager.

```bash
curl --cert client.pem --key client-key.pem --cacert ca.pem \
  https://localhost:4712/health
```

**Response:**
```json
{
  "status": "ok",
  "package_manager": "apt",
  "package_manager_version": "2.6.1",
  "is_upgrading": false
}
```

### `GET /status`

Returns the current system status.

```bash
curl --cert client.pem --key client-key.pem --cacert ca.pem \
  https://localhost:4712/status
```

**Response:**
```json
{
  "message": "System has 2 outdated packages",
  "updates": ["libc6", "vim"],
  "is_upgrading": false,
  "daemon_version": "0.3.0",
  "allow_reboot": false,
  "auto_clean": true,
  "auto_remove": false
}
```

**Status codes:**

- `200 OK`: Status returned.
- `412 PRECONDITION_FAILED`: The detected package manager is not available on this system (e.g., no `apt` or `brew` found).
- `500 INTERNAL_SERVER_ERROR`: Checking for updates failed.

### `POST /packages/full-upgrade`

Triggers a full system upgrade (e.g., `apt full-upgrade -y` or `brew upgrade`). This operation is asynchronous; the endpoint returns immediately while the upgrade runs in the background.

**Request body:**
```json
{
  "dry_run": false
}
```

With `dry_run: true`, the daemon simulates the upgrade and returns the list of packages that would be changed instead:

```json
{
  "message": "dry-run completed",
  "updates": ["libc6", "vim"]
}
```

**Response (non-dry-run):**
```json
{
  "message": "full upgrade triggered"
}
```

Only one upgrade can run at a time; the `is_upgrading` flag in `GET /status` reports whether an upgrade is in progress.

**Status codes:**

- `200 OK`: Upgrade triggered (or dry-run completed).
- `412 PRECONDITION_FAILED`: The package manager is not available, or a full upgrade is already running.

### `POST /node/reboot`

Reboots the host (via `sudo systemctl reboot`, falling back to `sudo reboot`). Only allowed if `features.allow_reboot` is enabled in the configuration, and rejected while a full upgrade is running. The request body is optional and currently ignored.

**Response when allowed:**
```json
{
  "message": "reboot triggered"
}
```

**Response when disabled:**
```json
{
  "message": "reboot is not enabled on this daemon"
}
```

**Status codes:**

- `200 OK`: Reboot triggered.
- `403 FORBIDDEN`: Reboot is not enabled (`features.allow_reboot` is `false`).
- `412 PRECONDITION_FAILED`: The package manager is not available.
- `423 LOCKED`: A full upgrade is currently running.

## Development

### Running Tests

```bash
cargo test
```

*Note: Some tests are platform-specific and may behave differently on non-Linux systems.*
