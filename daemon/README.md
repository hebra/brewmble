# Brewmble Daemon

The Brewmble Daemon (`brewmbled`) is a background service that runs on managed nodes. It provides a REST API for system status and package management, supporting multiple backends like APT (Linux) and Homebrew (macOS).

## Table of Contents

- [Features](#features)
- [Installation](#installation)
  - [From Source](#from-source)
  - [Using Docker/Podman](#using-dockerpodman)
  - [Running as a systemd service (Linux)](#running-as-a-systemd-service-linux)
  - [Running as a Launch Agent (macOS)](#running-as-a-launch-agent-macos)
  - [Sudo Configuration](#sudo-configuration)
- [Configuration](#configuration)
- [Security and Authentication](#security-and-authentication)
- [API Endpoints](#api-endpoints)
  - [`GET /status`](#get-status)
  - [`POST /packages/full-upgrade`](#post-packagesfull-upgrade)
- [Development](#development)
  - [Running Tests](#running-tests)

## Features

- **mDNS Registration**: Automatically announces itself on the local network as `_brewmble._tcp`.
- **Multi-backend Support**: Automatically detects and uses the available package manager (APT or Homebrew).
- **System Status**: Reports whether the system is up-to-date and lists available updates.
- **Package Management**: Can trigger a full system upgrade via the detected package manager.
- **Authentication**: Secure access via API keys.
- **Port Hunting**: Automatically finds an available port starting from 8080 if not specified.

## Installation

### From Source

```bash
cargo build --release
```

The binary will be located at `target/release/brewmbled`.

### Using Docker/Podman

A `Containerfile` is provided for building a container image:

```bash
podman build -t brewmbled .
podman run -d --net=host --cap-add=CAP_SYS_ADMIN brewmbled
```

*Note: `--net=host` is required for mDNS discovery, and `CAP_SYS_ADMIN` (or equivalent) may be needed for APT operations depending on your configuration.*

### Running as a systemd service (Linux)

For Linux systems, a sample systemd service file is provided in the `docs` folder.

#### Setup

1. **Copy the service file**:
   ```bash
   sudo cp ../docs/brewmbled.service.sample /etc/systemd/system/brewmbled.service
   ```

2. **Configure the service**:
   Open the file in an editor to adjust the `ExecStart` path and any environment variables (like `BREWMBLE_DAEMON_API_KEY`):
   ```bash
   sudo nano /etc/systemd/system/brewmbled.service
   ```

3. **Reload systemd**:
   ```bash
   sudo systemctl daemon-reload
   ```

#### Managing the Service

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

### Running as a Launch Agent (macOS)

For macOS, you can run `brewmbled` as a background service for the logged-in user using `launchd`. A sample plist file is provided in the `docs` folder.

#### Setup

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

#### Managing the Service

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

### Sudo Configuration

The `brewmbled` daemon runs as the `brewmble` user but needs to perform package management operations that require root privileges. To enable this, you must configure `sudo` to allow the `brewmble` user to run `apt` commands without a password.

1. **Create a sudoers file**:
   It is recommended to create a separate file in `/etc/sudoers.d/`:
   ```bash
   sudo nano /etc/sudoers.d/brewmble
   ```

2. **Add the following content**:
   ```text
   brewmble ALL=(root) NOPASSWD: /usr/bin/apt, /usr/bin/apt-get
   ```

3. **Set correct permissions**:
   The file must have strict permissions:
   ```bash
   sudo chmod 440 /etc/sudoers.d/brewmble
   ```

## Configuration

Environment variables can be used for configuration:

- `BREWMBLE_DAEMON_PORT`: Port to listen on.
- `BREWMBLE_DAEMON_HOSTNAME`: Hostname to use for mDNS registration.
- `BREWMBLE_DAEMON_IP`: Explicit IP address to use for mDNS registration.
- `BREWMBLE_DAEMON_API_KEY`: API key for authentication. If not provided, one will be generated on startup and printed to the logs.
- `BREWMBLE_APT_UPDATE_INTERVAL`: Interval in minutes between `apt-get update` calls (Linux only). Defaults to 360 (6 hours). Set to 0 to force an update on every status check.
- `BREWMBLE_BREW_UPDATE_INTERVAL`: Interval in minutes between `brew update` calls (macOS only). Defaults to 360 (6 hours). Set to 0 to force an update on every status check.
- `RUST_LOG`: Logging level (e.g., `info`, `debug`).

## Security and Authentication

All API endpoints require authentication via an `X-API-Key` header.

```bash
curl -H "X-API-Key: your-secret-api-key" http://localhost:8080/status
```

For more advanced security options like HTTPS or SSH tunneling, see the [main Security section](../README.md#security).

## API Endpoints

### `GET /status`

Returns the current system status.

**Response:**
```json
{
  "message": "System has 2 outdated packages",
  "updates": ["libc6", "vim"],
  "is_upgrading": false
}
```

### `POST /packages/full-upgrade`

Triggers a full system upgrade (e.g., `apt full-upgrade -y` or `brew upgrade`). This operation is asynchronous.

**Response:**
```json
{
  "message": "full upgrade triggered"
}
```

## Development

### Running Tests

```bash
cargo test
```

*Note: Some tests are platform-specific and may behave differently on non-Linux systems.*
