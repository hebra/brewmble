# Project Brief: Brewmble

## Vision
Brewmble aims to be the "fleet manager" for small-scale computer clusters, home labs, and distributed systems. It simplifies the tedious task of keeping multiple machines up-to-date by providing a centralized, automated, and easy-to-use interface for package management across Linux and macOS.

## Problem Statement
Managing updates for a handful of Raspberry Pis, home servers, or work machines often involves SSH-ing into each one individually. This is time-consuming, error-prone, and lacks a unified view of the system's health and update status. Existing enterprise solutions (like Ansible, Puppet, or specialized fleet managers) are often too heavy or complex for small-scale environments.

## Target Audience
- **Home Lab Enthusiasts**: Users running multiple Raspberry Pis or small servers.
- **Small Teams**: Developers managing a small set of shared build servers or staging environments.
- **Individual Power Users**: People with multiple machines across different operating systems (Linux/macOS).

## Key Pillars
1.  **Simplicity**: Zero-config discovery via mDNS. Small, efficient binaries written in Rust.
2.  **Centralization**: View and manage the update status of the entire fleet from a single CLI or Web interface.
3.  **Cross-Platform**: Unified management for systems using different package managers (APT on Linux, Homebrew on macOS).
4.  **Security**: Lightweight authentication and clear paths for securing communication via standard networking tools.

## Architecture
Brewmble follows a distributed agent-based architecture:
-   **Brewmble Daemon (`brewmbled`)**: A lightweight agent running on every managed node. It abstracts the local package manager and exposes a secure REST API. See [Architecture](./ARCHITECTURE.md) for details.
-   **Brewmble CLI (`brewmble`)**: The primary control point for users to discover nodes, check status, and trigger updates across the fleet.
-   **mDNS Service Discovery**: Enables nodes to find each other and the CLI to find nodes without manual IP configuration.
-   **Brewmble Web (Planned)**: A dashboard for visual monitoring and management.
-   **Brewmble REST (Shared)**: A common crate defining the API specification and shared data models.
