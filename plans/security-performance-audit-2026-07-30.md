# Brewmble Security & Performance Audit

**Date:** 2026-07-30
**Scope:** `daemon/`, `cli/`, `rest/` crates, `daemon/Containerfile`, `daemon/Makefile`, `docs/brewmbled.service.sample`
**Auditor:** Kilo

This document records the findings of a codebase-wide security and performance review, plus a concrete roadmap for remediation. Items are grouped by theme and severity.

---

## Executive Summary

The project has a clean command-execution model (no shelling out to user input), good separation between CLI and daemon via the `rest` crate, and reasonable authentication coverage. The most significant gaps are:

1. **Blocking I/O inside the async daemon** — every `/status` call runs synchronous subprocesses on the tokio runtime, which can starve all worker threads.
2. **No transport security or bind-address control** — the API key is sent in cleartext over HTTP on `0.0.0.0`, and mDNS advertises the service to the LAN.
3. **Unsanitized network-derived strings** — mDNS TXT values flow into the terminal and into a persisted config file, enabling terminal escape injection.
4. **API key lifecycle issues** — keys are logged, passed as CLI arguments, and stored in a world-readable config file by default.

The roadmap at the end of this document gives fix suggestions for each finding, ordered by impact.

---

## Security Findings

### High Severity

#### 1. Cleartext API key over HTTP on `0.0.0.0` with mDNS discovery

- **Where:** `daemon/src/main.rs:96-118`, `rest/src/lib.rs:3`, `cli/src/main.rs:1280-1303` (always uses `http://`)
- **Risk:** The daemon binds to all interfaces by default and advertises itself via mDNS. The `X-API-Key` header is sent in cleartext, so any device on the local network can capture it and trigger upgrades or reboots.
- **Mitigation today:** Provide a strong, generated UUID key and document the LAN-trust assumption.
- **Fix:**
  - Add `--bind-address` / `BREWMBLE_DAEMON_BIND_ADDRESS` (default to `127.0.0.1` for development or `0.0.0.0` only when explicitly requested).
  - Document the threat model in `README.md` and `AGENTS.md`.
  - Long-term: support TLS with pinned self-signed certs or mutual TLS between CLI and daemon.

#### 2. Terminal escape injection via mDNS TXT records

- **Where:** `cli/src/main.rs:887-894` (`entry_id`), `cli/src/main.rs:149-185` (`merge_nodes`), `cli/src/main.rs:1084-1086` (`run_status` output)
- **Risk:** A malicious device on the LAN can advertise a `_brewmble._tcp` service with ANSI / OSC escape sequences in the `id` TXT property. This value is printed to the terminal and persisted as a node `name` in `~/.brewmble.yaml`, so the injection survives across CLI invocations.
- **Fix:**
  - Add a `sanitize_for_terminal_and_config(s: &str) -> String` helper that strips control characters (`0x00-0x1F` except `\t`, `\n`, `\r`, plus `0x7F` and any CSI/OSC sequences).
  - Apply it to `entry_id()` and any other network-derived value before storage or display.
  - Consider also stripping or warning when a config-loaded node name contains control characters.

#### 3. Non-constant-time API key comparison

- **Where:** `daemon/src/main.rs:201`
- **Risk:** `key == state.api_key` short-circuits on the first differing byte, creating a timing side-channel. Low practical risk on a LAN, but trivial to fix correctly.
- **Fix:** Use a constant-time comparison such as `subtle::ConstantTimeEq`:
  ```rust
  use subtle::ConstantTimeEq;
  let ok = key.as_bytes().ct_eq(state.api_key.as_bytes()).into();
  ```

#### 4. API key exposure in logs, command lines, and shell history

- **Where:**
  - `daemon/src/main.rs:132` — generated key logged via `info!`.
  - `daemon/src/main.rs:49` — `--api-key` accepted as CLI arg / env var (visible in `ps` / `/proc/<pid>/environ`).
  - `cli/src/main.rs:289` — `profile set-key <node> <key>` takes key as positional argument (shell history, `ps`).
- **Risk:** Local users can read the key from process listings, journald, or shell history.
- **Fix:**
  - Do **not** log the generated key at `info` level; print it once to stderr during first startup and/or write it to a `0600` file such as `/var/lib/brewmble/.api_key`.
  - Prefer `--api-key-file` / `BREWMBLE_DAEMON_API_KEY_FILE` over env/CLI args.
  - Change `profile set-key` to read the key from stdin when no positional argument is supplied (e.g., `brewmble profile set-key mynode` prompts).
  - For generated keys, consider writing to a `brewmble` data dir and emit only the file path in logs.

---

### Medium Severity

#### 5. Config file created world-readable and non-atomically

- **Where:** `cli/src/main.rs:134-138` (`save_config`)
- **Risk:** `fs::write` creates files with `0666 & ~umask` (typically `0644`). If the config contains a plaintext `api_key`, other local users can read it. Also, a crash during write can leave a truncated config.
- **Fix:**
  ```rust
  use std::fs::{self, OpenOptions};
  use std::os::unix::fs::OpenOptionsExt;

  let tmp = path.with_extension("yaml.tmp");
  let mut file = OpenOptions::new()
      .write(true)
      .create(true)
      .truncate(true)
      .mode(0o600)
      .open(&tmp)?;
  file.write_all(content.as_bytes())?;
  drop(file);
  fs::rename(tmp, path)?;
  ```
  On non-Unix, use platform-specific private-file APIs or at least set read-only for the current user.

#### 6. Unauthenticated `/health` leaks host fingerprint

- **Where:** `daemon/src/main.rs:191,206-213`
- **Risk:** `/health` is intentionally public but returns package manager name and version, which discloses OS family (brew → macOS, apt → Debian). Combined with mDNS, this helps an attacker pick exploits.
- **Fix:** Either require auth on `/health` or trim the response to `status: ok` plus a boolean `upgrading`. If detailed info is needed for diagnostics, put it behind `/status`.

#### 7. Reboot path is fragile and requires passwordless sudo

- **Where:** `daemon/src/main.rs:380-394`
- **Risk:**
  - `sudo` without `-n` may hang or fail depending on sudoers config.
  - The fallback to `sudo reboot` only runs if spawning `sudo systemctl reboot` fails, not if it exits non-zero.
  - Requiring passwordless sudo for reboot/upgrade widens the blast radius if the daemon is compromised.
- **Fix:**
  - Always use `sudo -n` (non-interactive).
  - Inspect `ExitStatus` of both commands; only report success if one actually succeeded.
  - Consider polkit / a dedicated capability instead of broad sudo access; at minimum document the exact sudoers lines required.
  - Respect `RebootRequest.delay_secs` or remove the field from the API.

#### 8. No rate limiting and no key-strength check

- **Where:** `daemon/src/main.rs:185-204`
- **Risk:** Unlimited 401 responses allow online brute-force of weak user-supplied keys.
- **Fix:**
  - Enforce a minimum key length (e.g., 32 bytes) for user-supplied keys.
  - Add a per-source IP failed-attempt counter with exponential backoff, or use a crate like `governor` on the auth layer.
  - Consider `X-API-Key` rotation endpoints.

#### 9. `is_upgrading` flag can wedge permanently

- **Where:** `daemon/src/main.rs:327-335`
- **Risk:** If the spawned upgrade task panics, `is_upgrading` is never reset, so upgrades and reboots are rejected until the daemon restarts (a self-DoS).
- **Fix:** Use a drop guard or `defer!` to reset the flag even on panic:
  ```rust
  struct UpgradingGuard(Arc<AtomicBool>);
  impl Drop for UpgradingGuard {
      fn drop(&mut self) {
          self.0.store(false, Ordering::SeqCst);
      }
  }
  ```

---

### Low Severity

#### 10. `RebootRequest.delay_secs` is accepted but ignored

- **Where:** `daemon/src/main.rs:348`
- **Fix:** Implement the delay using `tokio::time::sleep` before invoking reboot, or remove the field from the API schema.

#### 11. Container image issues

- **Where:** `daemon/Containerfile`, `daemon/Makefile`
- **Issues:**
  - `apt full-upgrade -s` at build time is a simulation that does nothing but can fail the build.
  - No `USER` directive; the daemon runs as root inside the container.
  - The Makefile's `container` target uses `--network bridge`, which breaks mDNS multicast discovery.
- **Fix:**
  - Remove the simulated upgrade or replace it with a real, pinned `apt-get upgrade -y` if required.
  - Add a non-root `USER brewmble` after installing packages; ensure file permissions still allow the binary to run.
  - Document that mDNS needs host networking (`--network host` on Linux) or an mDNS reflector; the `5353` port mapping is misleading.

#### 12. Keyring entries are orphaned when addresses change

- **Where:** `cli/src/main.rs:74,168-170`
- **Risk:** `merge_nodes` updates a node's address on re-discovery but leaves the old keyring entry behind. The next request then uses `get_api_key()` against the new address, finds nothing, and gets 401.
- **Fix:** When a node's address changes, migrate the keyring entry from the old address to the new one (or at least warn the user).

#### 13. `EDITOR` with arguments is not supported

- **Where:** `cli/src/main.rs:405-406`
- **Risk:** None — `Command::new` is used without a shell, so there is no injection. It is a functional bug (e.g., `EDITOR="code --wait"` fails).
- **Fix:** Parse the `EDITOR` value with `shlex` or document that only a single binary name is supported.

---

## Performance Findings

### High Severity

#### P1. Blocking subprocess calls inside async handlers

- **Where:** `package_manager/mod.rs:26-28` (`RealCommandRunner`), `apt.rs`, `brew.rs`, `daemon/src/main.rs:237` (`status_handler`)
- **Risk:** `std::process::Command::output()` blocks the calling tokio worker thread. `/status` can therefore hold a worker for seconds (`apt-get update` + `apt-get -s dist-upgrade`); a background upgrade holds a worker for minutes. With enough concurrent requests, all workers are blocked and the HTTP server starves.
- **Fix:**
  - Use `tokio::process::Command` instead of `std::process::Command`, **or**
  - Wrap the blocking runner in `tokio::task::spawn_blocking`:
    ```rust
    tokio::task::spawn_blocking(move || runner.run(program, args)).await??
    ```
  - Apply the same pattern to `version()`, `is_available()`, and cleanup commands.

#### P2. Process spawns on every request for static data

- **Where:** `daemon/src/main.rs:210` (`health_handler` calls `pm.version()`), `daemon/src/main.rs:219/277/350` (`is_available()` called per request)
- **Risk:** Every health check and every protected endpoint spawns one or more subprocesses. For brew, `is_available()` can attempt three different paths.
- **Fix:** Compute availability and version once at daemon startup and store them in `AppState`. Re-check only when explicitly asked (e.g., `SIGHUP`) or at most once per minute.

#### P3. No caching of expensive update-list queries and no single-flight

- **Where:** `apt.rs:111-137`, `brew.rs:111-131`
- **Risk:** The TTL cache only covers `apt-get update` / `brew update`. `apt-get -s dist-upgrade` and `brew outdated` run on every `/status`. Concurrent requests race and contend on the apt lock, producing 500s.
- **Fix:**
  - Cache the final `Vec<String>` updates list for the TTL window, keyed by profile flags.
  - Use a single-flight guard (e.g., `tokio::sync::Mutex` around the cache-refresh path, or `async_singleflight`) so only one refresh runs at a time.
  - Invalidate the cache on a successful `full_upgrade`.

---

### Medium Severity

#### P4. Mutex poisoning in update-cache mutex

- **Where:** `apt.rs:73`, `brew.rs:81`
- **Risk:** `last_update.lock().unwrap()` can panic if another task panicked while holding the lock, turning a single failure into a permanent 500.
- **Fix:** Use `Mutex::lock().unwrap_or_else(|poisoned| poisoned.into_inner())`.

#### P5. CLI `run_node` and `run_version` are sequential

- **Where:** `cli/src/main.rs:918-954`, `cli/src/main.rs:1342-1413`
- **Risk:** N targets with a 60s timeout each can take N×60s if nodes are offline.
- **Fix:** Parallelize with the same rayon/thread-pool pattern already used in `run_status`.

#### P6. No HTTP-layer timeouts or connection limits

- **Where:** `daemon/src/main.rs:168-170`
- **Risk:** Slowloris / idle-connection exhaustion on an exposed port; graceful shutdown does not wait for the in-flight upgrade task, which can leave apt/dpkg in an inconsistent state.
- **Fix:**
  - Add `tower_http::timeout::TimeoutLayer` with a generous request timeout (e.g., 5 minutes for status, longer for upgrade endpoints if they were synchronous; but upgrade is already fire-and-forget).
  - Add `DefaultBodyLimit` to cap request bodies.
  - On shutdown, signal the upgrade task and wait for it to finish before exiting.

---

### Low Severity

#### P7. `resolve_url` mishandles bare IPv6 addresses

- **Where:** `cli/src/main.rs:1280-1303`
- **Risk:** `::1` is mis-parsed as host `:` and port `1`, producing `http://[:]:1`.
- **Fix:** Detect bare IPv6 using `target.parse::<IpAddr>()`; if it parses as IPv6 and has no port, emit `http://[addr]`.

#### P8. Discovery thread uses a hardcoded 5s timeout

- **Where:** `cli/src/main.rs:1016`
- **Risk:** Ignores `BREWMBLE_TIMEOUT` when `status --all` is used.
- **Fix:** Use `get_default_timeout()` or a dedicated discover timeout CLI flag.

#### P9. Native progress sleep adds 500ms to every status run

- **Where:** `cli/src/main.rs:1150`
- **Risk:** Minor latency; also sleeps the rayon scope's main thread.
- **Fix:** Make the sleep optional or shorten it.

#### P10. Duplicate target queries before deduplication

- **Where:** `cli/src/main.rs:973-988`
- **Risk:** `initial_targets` dedups by `(addr, name)`, but `seen_targets` dedups by `addr` only, so the same node under two names is queried once but only after spawning a worker.
- **Fix:** Dedupe by address up front.

---

## Roadmap: Recommended Remediation Order

This roadmap assumes small, reviewable PRs. Each row links a finding to a concrete change and a suggested priority.

| # | Task | Files to change | Priority | Notes |
|---|------|-----------------|----------|-------|
| 1 | Move all subprocess execution off tokio workers | `package_manager/mod.rs`, `apt.rs`, `brew.rs` | **Critical** | Use `tokio::process` or `spawn_blocking`. Tests with `MockRunner` stay unchanged if the runner trait remains sync and is wrapped. |
| 2 | Cache `version()` and `is_available()` in `AppState` | `daemon/src/main.rs` | **Critical** | Compute once at startup; removes per-request process spawns. |
| 3 | Cache full update-list and add single-flight refresh | `apt.rs`, `brew.rs` | **High** | Eliminates apt lock contention and redundant `dist-upgrade`/`outdated` runs. |
| 4 | Sanitize network-derived strings before display/storage | `cli/src/main.rs` | **High** | Fixes terminal escape injection from mDNS. |
| 5 | Constant-time API key comparison | `daemon/src/main.rs` | **High** | Add `subtle` to `daemon/Cargo.toml`. |
| 6 | Stop logging generated API key; write to restricted file | `daemon/src/main.rs` | **High** | Security vs. convenience trade-off; at minimum log only the file path, not the key. |
| 7 | Restrict config file permissions and make writes atomic | `cli/src/main.rs` | **High** | `0600` + temp-file + rename. |
| 8 | Add bind-address option and document trust model | `daemon/src/main.rs`, `README.md`, `AGENTS.md` | **High** | First hardening step before TLS. |
| 9 | Harden `/health` response | `daemon/src/main.rs` | **Medium** | Remove version string; keep minimal `ok`/`upgrading`. |
| 10 | Fix reboot command logic and use `sudo -n` | `daemon/src/main.rs` | **Medium** | Inspect exit status; document required sudoers lines. |
| 11 | Guard `is_upgrading` against panic | `daemon/src/main.rs` | **Medium** | Drop guard resets flag on panic. |
| 12 | Add rate-limit / key-strength check | `daemon/src/main.rs` | **Medium** | Minimum length 32; per-IP backoff. |
| 13 | Parallelize `run_node` and `run_version` | `cli/src/main.rs` | **Medium** | Reuse rayon pattern from `run_status`. |
| 14 | Fix mutex poisoning | `apt.rs`, `brew.rs` | **Medium** | `poisoned.into_inner()`. |
| 15 | Add HTTP timeouts and body limits | `daemon/src/main.rs` | **Medium** | `TimeoutLayer` + `DefaultBodyLimit`. |
| 16 | Fix `resolve_url` bare IPv6 handling | `cli/src/main.rs` | **Low** | Also update unit tests. |
| 17 | Fix container image | `daemon/Containerfile`, `daemon/Makefile` | **Low** | Remove simulated upgrade, add non-root user, document host networking for mDNS. |
| 18 | Migrate keyring entries on address change | `cli/src/main.rs` | **Low** | Quality-of-life fix for `discover -u`. |
| 19 | Respect timeout in discovery thread | `cli/src/main.rs` | **Low** | Use `get_default_timeout()`. |
| 20 | Implement or remove `RebootRequest.delay_secs` | `daemon/src/main.rs`, `rest/src/lib.rs` | **Low** | Decide API contract. |
| 21 | Make `profile set-key` read from stdin | `cli/src/main.rs` | **Low** | Avoid shell history exposure. |

---

## Positive Findings

These practices should be preserved:

- **No shell execution.** All subprocess calls use fixed programs with fixed argv; no user-controlled input reaches a shell.
- **Keyring support.** The CLI can store keys in the OS keyring and explicitly removes plaintext keys when doing so.
- **Uniform auth middleware.** All routes except `/health` go through `auth_middleware`; the 412 semantics for unsupported package managers are consistent.
- **Upgrade concurrency guard.** `compare_exchange` on `is_upgrading` correctly prevents concurrent upgrades.
- **Current dependencies.** Locked versions of axum, tokio, hyper, and reqwest are recent; no obvious CVEs are associated with these exact versions at audit time.

---

## Appendix: Quick Reference of Key File/Line Locations

| Concern | Location |
|---------|----------|
| Bind address / port hunting | `daemon/src/main.rs:95-118` |
| Auth middleware / key comparison | `daemon/src/main.rs:185-204` |
| Generated key logging | `daemon/src/main.rs:128-134` |
| Reboot command | `daemon/src/main.rs:380-394` |
| `is_upgrading` reset | `daemon/src/main.rs:327-335` |
| Health handler | `daemon/src/main.rs:206-213` |
| Status handler | `daemon/src/main.rs:215-271` |
| Config load/save | `cli/src/main.rs:108-138` |
| mDNS entry parsing | `cli/src/main.rs:887-916` |
| Node merge / discovery | `cli/src/main.rs:140-190`, `450-532` |
| URL resolution | `cli/src/main.rs:1280-1303` |
| Subprocess runner | `package_manager/mod.rs:26-28` |
| apt cache / update logic | `package_manager/apt.rs:70-137` |
| brew cache / update logic | `package_manager/brew.rs:79-131` |
| REST constants | `rest/src/lib.rs:1-56` |
