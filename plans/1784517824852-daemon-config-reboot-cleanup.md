# Daemon Config: Reboot, Auto-Clean, Auto-Remove

**Status: Completed.** All implementation steps below have been applied and tests pass in `rest`, `daemon`, and `cli`.

## Goal
Add three daemon-level startup configuration options to `brewmbled` and expose a dedicated reboot action in the CLI.

1. `--allow-reboot` / `BREWMBLE_DAEMON_ALLOW_REBOOT` — permits the daemon to reboot the host when explicitly requested via CLI.
2. `--auto-clean` / `BREWMBLE_DAEMON_AUTO_CLEAN` — automatically cleans downloaded packages after a successful upgrade (`apt-get autoclean` / `brew cleanup`).
3. `--auto-remove` / `BREWMBLE_DAEMON_AUTO_REMOVE` — automatically removes old/unused packages after a successful upgrade (`apt-get autoremove -y` / `brew autoremove`).

Also update the CLI:
- Rename `brewmble packages` to `brewmble node`.
- Keep `packages` as a visible alias for backward compatibility.
- Add `brewmble node --reboot` / `-r` as a standalone action, mutually exclusive with `--full-upgrade`.
- Add support for the new daemon reboot endpoint.

## Decisions
- All three flags default to `false` (opt-in).
- Cleanup steps run only after a successful `full_upgrade()`. Cleanup failures are logged as warnings and do not fail the whole upgrade.
- Reboot is a dedicated action triggered by the CLI, not automatic after upgrade.
- New daemon endpoint: `POST /node/reboot`.
- If `allow_reboot` is disabled, the endpoint returns `403 Forbidden`.
- If an upgrade is currently running, the reboot endpoint returns `423 Locked` to avoid corrupting the upgrade.
- Expose the three config flags in `StatusResponse` so the CLI/dashboard can show them.
- Homebrew mapping: `auto_clean` → `brew cleanup`; `auto_remove` → `brew autoremove`.

## Affected Files
- `rest/src/lib.rs` — add `PATH_REBOOT`, `RebootRequest`, `RebootResponse`, extend `StatusResponse`.
- `daemon/src/main.rs` — add CLI args, `AppState` fields, reboot handler, route wiring, pass config to package manager, upgrade-spawn cleanup.
- `daemon/src/package_manager/mod.rs` — extend `PackageManager` trait with cleanup methods.
- `daemon/src/package_manager/apt.rs` — implement cleanup/autoremove, consume config flags.
- `daemon/src/package_manager/brew.rs` — implement cleanup/autoremove, consume config flags.
- `cli/src/main.rs` — rename `Packages` to `Node`, add `--reboot`, add `run_reboot`.
- `README.md` — document new env vars.
- `daemon/README.md` — document new env vars, new endpoint, and updated sudoers rules (reboot requires `sudo systemctl reboot` or equivalent).
- `AGENTS.md` — add mandatory rule that plans must be saved in `plans/` folder.

## Implementation Steps

### [x] 1. REST crate
- Add `pub const PATH_REBOOT: &str = "/node/reboot";`.
- Add `RebootRequest` (empty or with optional `delay_secs`) and `RebootResponse { message: String }`.
- Extend `StatusResponse` with `pub allow_reboot: bool`, `pub auto_clean: bool`, `pub auto_remove: bool`.

### [x] 2. Daemon config & state
- Add to `Cli`:
  - `--allow-reboot` env `BREWMBLE_DAEMON_ALLOW_REBOOT`
  - `--auto-clean` env `BREWMBLE_DAEMON_AUTO_CLEAN`
  - `--auto-remove` env `BREWMBLE_DAEMON_AUTO_REMOVE`
- Add matching fields to `AppState`.
- Pass flags when constructing the package manager.

### [x] 3. Package manager trait & implementations
- Add to `PackageManager`:
  - `fn auto_clean(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>`
  - `fn auto_remove(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>`
- In `Apt::full_upgrade()`, after successful upgrade:
  - if `auto_clean`, call `self.runner.run("sudo", &["apt-get", "autoclean"])` and log warning on failure.
  - if `auto_remove`, call `self.runner.run("sudo", &["apt-get", "autoremove", "-y"])` and log warning on failure.
- In `Brew::full_upgrade()`, after successful upgrade:
  - if `auto_clean`, call `self.run_brew(&["cleanup"])`.
  - if `auto_remove`, call `self.run_brew(&["autoremove"])`.
- Store the flags in `Apt`/`Brew` structs for simplicity.

### [x] 4. Daemon reboot endpoint
- Add `reboot_handler` in `daemon/src/main.rs`:
  - Check `state.package_manager.is_available()`.
  - If `is_upgrading`, return `423 Locked`.
  - If `!state.allow_reboot`, return `403 Forbidden`.
  - Spawn async task that runs `sudo systemctl reboot` (or `sudo reboot`) and return `200 OK` immediately.
- Wire route: `.route(PATH_REBOOT, post(reboot_handler))`.

### [x] 5. CLI rename & reboot action
- Rename `Commands::Packages` to `Commands::Node`.
- Add `visible_alias = "packages"`.
- Change `full_upgrade` from `required = true` to optional.
- Add `reboot: bool` with short `-r`, long `--reboot`.
- Make `full_upgrade` and `reboot` mutually exclusive in logic; `run_node` errors if neither or both are specified.
- Keep `--dry-run` only valid with `--full-upgrade` via `conflicts_with` and manual checks.
- Implement reboot path in `run_node` to `POST /node/reboot` to each target and print result.

### [x] 6. Status response updates
- Update `daemon/src/main.rs` `status_handler` to populate the new booleans from `AppState`.
- Update `cli/src/main.rs` `query_status` to display them.

### [x] 7. Documentation
- Update main README env var table.
- Update daemon README config section and sudoers section to include `/usr/sbin/reboot`, `/bin/systemctl` or similar.
- Update `AGENTS.md` with mandatory rule that plans must be saved in `plans/` folder.

### [x] 8. Tests
- Add daemon unit tests for:
  - CLI parsing of new flags.
  - Reboot handler returns 403 when disabled.
  - Reboot handler returns 423 when upgrading.
  - Apt/Brew cleanup invocation after upgrade.
- Add CLI unit tests for:
  - `node --reboot` parsing.
  - Mutually exclusive `--full-upgrade` and `--reboot`.
  - Alias `packages --full-upgrade` still works.

## Validation
- `cd daemon && cargo test`
- `cd cli && cargo test`
- `cd rest && cargo test`
- Manual CLI parse check: `cargo run -- node --reboot localhost:8080` and `cargo run -- packages --full-upgrade localhost:8080` (alias).

## Risks & Open Questions
- **Sudoers:** Reboot requires passwordless sudo for the reboot command; document exact paths per distribution.
- **Homebrew autoremove:** Older Homebrew versions may not have `brew autoremove`; consider whether to tolerate command-not-found as a no-op or require recent Homebrew.
- **Reboot during upgrade:** Blocking reboot while `is_upgrading` is true prevents corruption but means a stuck upgrade blocks reboot.
