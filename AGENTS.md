# AGENTS.md

IMPORTANT: Keep all agent guidance in this file. DO NOT CREATE ANY VENDOR- OR MODE-SPECIFIC FILES IF THEY NOT ALREADY EXIST.

This file provides guidance to agents when working with code in this repository.

## Build/Test Commands

- CLI: `cd cli && cargo build/test/run`
- Daemon: `cd daemon && cargo build/test/run`
- Container: `cd daemon && make container` (uses podman by default, override with `CONTAINER_TOOL=docker`)
- Run single test: `cargo test test_name` (from cli/ or daemon/ directory)

## Non-Obvious Project Patterns

- Daemon APT functionality requires Linux systems (Debian-based with apt)
- Uses mDNS service discovery with "_brewmble._tcp.local." service type for automatic daemon discovery
- Environment variables control daemon configuration: BREWMBLE_DAEMON_PORT (default 4712), BREWMBLE_DAEMON_HOSTNAME, BREWMBLE_DAEMON_IP, BREWMBLE_DAEMON_API_KEY, BREWMBLE_APT_UPDATE_INTERVAL (default 360), BREWMBLE_BREW_UPDATE_INTERVAL (default 360)
- BREWMBLE_TIMEOUT env var accepts both seconds (integer) or humantime format (e.g., "1m", "30s")
- Daemon caches 'apt-get update' and 'brew update' results based on their respective update intervals (default 360 mins) - see get_updates()
- CLI uses blocking HTTP client (reqwest with blocking feature) while daemon uses async Axum framework
- Different Rust editions: CLI uses 2021, daemon uses 2024
- Container builds use podman with ports 4712 (HTTP) and 5353 (mDNS)
- Daemon auto-hunts for free port starting from 4712 if BREWMBLE_DAEMON_PORT not set
- API authentication uses X-API-Key header (not Authorization header)
- If no API key provided, daemon generates UUID v4 and logs it

## Project Coding Rules (Non-Obvious Only)

- Use mDNS service registration patterns from daemon/src/main.rs for service discovery
- Linux-specific conditional compilation with #[cfg(target_os = "linux")] for apt functionality
- CLI uses blocking HTTP client (reqwest with blocking feature) while daemon uses async
- Service discovery timeout handling with flume channels (see cli/src/main.rs discover_targets)
- TabWriter for formatted CLI output with custom padding (2 spaces)
- IPv6 addresses in URLs must be wrapped in brackets: `http://[::1]:4712` (see resolve_url function)
- mDNS instance name format: "brewmbled-{hostname}" where hostname is first part before dot
- Daemon uses AtomicBool for is_upgrading state to prevent concurrent upgrades
- Full upgrade spawns tokio task and returns immediately (fire-and-forget pattern)
- Aim for at least 90% test coverage for all new code and major refactors

## Project Debug Rules (Non-Obvious Only)

- mDNS service registration failures logged with detailed error messages in daemon
- Linux-specific apt functionality debugging requires Debian-based system
- Container networking requires ports 4712 (HTTP) and 5353 (mDNS) to be exposed
- Daemon status endpoint provides JSON response with update details
- Tests use #[cfg(target_os = "macos")] to handle platform-specific behavior
- CLI discover command uses HashSet to deduplicate services by fullname

## Project Documentation Rules (Non-Obvious Only)

- CLI discovers daemons via mDNS, daemon serves status via HTTP API
- Daemon runs on Linux (Debian-based systems with apt) and macOS (with Homebrew)
- Environment variables configure daemon networking and identity
- REST and web components are planned but not yet implemented (empty directories)
- Container builds require both HTTP (4712) and mDNS (5353) ports

## Project Architecture Rules (Non-Obvious Only)

- Multi-component system: CLI discovers via mDNS, daemon serves HTTP status API
- Daemon architecture supports Linux (Debian-based) for apt and macOS for Homebrew
- Environment-based configuration replaces traditional config files
- mDNS service discovery enables automatic cluster discovery
- Container architecture requires both HTTP and mDNS networking
- Daemon uses middleware pattern for authentication (auth_middleware)
- Status handler returns 412 PRECONDITION_FAILED on systems without a supported package manager

## Project Planning Rules (Non-Obvious Only)

- Save all implementation plans to the `plans/` folder at the repository root. Do not use hidden or vendor-specific plan directories.
