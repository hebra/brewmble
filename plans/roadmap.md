# Brewmble Roadmap

This roadmap outlines the planned development for Brewmble, categorized by priority and timeframe.

## Phase 1: Foundation, Security & Stability (Short-term)
Focus on solidifying the core architecture, security posture, and developer/operator experience.

- [x] **Unified REST Crate**: Extract API models, shared constants, and common logic into `rest/` as a shared Rust library used by both CLI and Daemon.
- [x] **Daemon for macOS**: Full Homebrew support alongside Linux APT, enabling uniform multi-platform node management.
- [x] **Mutual TLS & PKI Infrastructure**: Cluster CA and client certificate management via OS keyring (`brewmble ca`, `brewmble cert`), replacing legacy API keys with mTLS authentication.
- [x] **Security Hardening**: TOFU certificate fingerprint pinning (`--repin`), mDNS output sanitization, drop-guarded upgrade state, non-interactive reboot execution (`sudo -n`), atomic configuration writes with restricted permissions (0600), and configuration validation (`brewmbled config validate`).
- [x] **Node Management & Remote Reboot**: Configurable post-upgrade cleanup (`auto_clean`, `auto_remove`), remote reboot capabilities (`POST /node/reboot` and `brewmble node --reboot`), and profile node name resolution (`brewmble node --list`).
- [x] **Subprocess Offloading & Resilience**: Non-blocking package manager subprocess execution (`tokio::task::spawn_blocking`), HTTP timeouts (`TimeoutLayer`), and startup caching of static daemon metadata.
- [x] **Dry-run Support**: Allow users to preview package updates (`--dry-run`) without applying changes.
- [x] **Comprehensive Logging & Health Checks**: Structured logging via `tracing` and dedicated health-check endpoint (`GET /health`).

## Phase 2: Performance & Feature Expansion (Medium-term)
Broadening functionality, ecosystem support, and query performance.

- [ ] **Parallel CLI Operations**: Parallelize `brewmble node` and `brewmble version` across targets using worker pools (matching `brewmble status` concurrency).
- [ ] **Daemon Update-List Caching & Single-Flight**: Cache resolved update lists within the TTL window and serialize concurrent refreshes to eliminate APT lock contention and redundant command execution.
- [ ] **Web Dashboard (MVP)**: Develop a lightweight web interface in the `web/` directory to visualize node statuses and trigger updates from a browser.
- [ ] **Additional Package Managers**: Add support for `dnf` (Fedora/RHEL), `pacman` (Arch), and `apk` (Alpine) to the daemon.
- [ ] **Filtering & Grouping**: Add CLI capabilities to target specific groups or tags of nodes (e.g., `brewmble status --group raspberry-pis`).
- [ ] **Automatic Updates**: Implement an optional "auto-pilot" mode in the daemon for scheduled unattended updates.
- [ ] **Rust Toolchain Updates**: Add `rustup` support so the daemon can update the Rust toolchain used for its own builds.
- [ ] **Self-Update**: Enable the CLI and daemon to update themselves to the latest release binary.

## Phase 3: Advanced Features & Ecosystem (Long-term)
Scaling cluster management and integrating with external infrastructure.

- [ ] **Plugin System**: Allow users to write custom pre-update and post-update hooks or scripts.
- [ ] **Notification Engine**: Integration with Slack, Discord, webhooks, or email to notify operators of update outcomes.
- [ ] **Secure Discovery**: Implement encrypted mDNS or alternative authenticated discovery mechanisms for untrusted network environments.
- [ ] **Rollback Capability**: Explore automated rollback workflows for package managers with snapshot/history support.
- [ ] **Cross-node Orchestration**: Coordinate multi-node update sequences (e.g., rolling restarts of services across a cluster after kernel updates).
