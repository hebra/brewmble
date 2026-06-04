# Brewmble Daemon

The Brewmble Daemon (`brewmbled`) is a background service that runs on managed nodes. It provides a REST API for system status and package management, supporting multiple backends like APT (Linux) and Homebrew (macOS).

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

1.  Copy the sample file to `/etc/systemd/system/`:
    ```bash
    sudo cp ../docs/brewmbled.service.sample /etc/systemd/system/brewmbled.service
    ```
2.  Edit the file to set the correct path to your `brewmbled` binary and any environment variables:
    ```bash
    sudo nano /etc/systemd/system/brewmbled.service
    ```
3.  Reload systemd, enable and start the service:
    ```bash
    sudo systemctl daemon-reload
    sudo systemctl enable brewmbled
    sudo systemctl start brewmbled
    ```

## Configuration

Environment variables can be used for configuration:

- `BREWMBLE_DAEMON_PORT`: Port to listen on.
- `BREWMBLE_DAEMON_HOSTNAME`: Hostname to use for mDNS registration.
- `BREWMBLE_DAEMON_IP`: Explicit IP address to use for mDNS registration.
- `BREWMBLE_DAEMON_API_KEY`: API key for authentication. If not provided, one will be generated on startup and printed to the logs.
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
