# Documentation

This directory contains documentation and sample configuration files for Brewmble.

## Systemd Configuration

To run the `brewmbled` daemon as a system service on Linux, you can use the provided sample file `brewmbled.service.sample`.

### Setup

1. **Copy the service file**:
   ```bash
   sudo cp docs/brewmbled.service.sample /etc/systemd/system/brewmbled.service
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

### Managing the Service

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
