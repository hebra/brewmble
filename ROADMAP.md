# Cobbler Roadmap

This roadmap outlines the planned development for Cobbler, categorized by priority and timeframe.

## Phase 1: Foundation & Stability (Short-term)
Focus on solidifying the current CLI/Daemon interaction and improving the developer experience.

- [ ] **Unified REST Crate**: Extract API models and common logic into the `rest/` directory as a shared Rust library used by both CLI and Daemon.
- [ ] **Daemon for macOS**: Expand the daemon to fully support macOS (Homebrew), allowing macOS nodes to be managed just like Linux nodes.
- [ ] **Comprehensive Logging**: Implement structured logging (e.g., using `tracing`) in the daemon to facilitate debugging and auditing.
- [ ] **Config Management**: Improve CLI configuration handling, including support for multiple profiles and better credential storage.
- [ ] **Health Checks**: Add a dedicated health-check endpoint to the daemon for monitoring tools.

## Phase 2: Feature Expansion (Medium-term)
Adding more functionality and broadening the ecosystem.

- [ ] **Web Dashboard (MVP)**: Develop a basic web interface in the `web/` directory to visualize node statuses and trigger updates from a browser.
- [ ] **Additional Package Managers**: Add support for `dnf` (Fedora/RHEL) and `pacman` (Arch) to the daemon.
- [ ] **Dry-run Support**: Allow users to see what updates *would* be applied without actually executing them.
- [ ] **Filtering & Grouping**: Add CLI capabilities to target specific groups of nodes (e.g., `cobbler status --group raspberry-pis`).
- [ ] **Automatic Updates**: Implement an optional "auto-pilot" mode in the daemon for scheduled updates.

## Phase 3: Advanced Features & Ecosystem (Long-term)
Scaling the project and adding advanced management capabilities.

- [ ] **Plugin System**: Allow users to write custom scripts or hooks that run before/after updates.
- [ ] **Notification Engine**: Integration with Slack, Discord, or Email to notify users of successful or failed updates.
- [ ] **Secure Discovery**: Implement encrypted mDNS or an alternative discovery mechanism for untrusted networks.
- [ ] **Rollback Capability**: Explore the possibility of rolling back updates for package managers that support it.
- [ ] **Cross-node Orchestration**: Coordinate updates across the cluster (e.g., rolling restarts of services after a kernel update).
