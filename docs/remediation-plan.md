# Remediation Plan

## Priority 1 — Critical / High (security-relevant)

### 1.1 `DEBIAN_FRONTEND` stripped by sudo → upgrade hangs
- Type: Security/Reliability — privilege-boundary env handling
- File: `daemon/src/package_manager/apt.rs` (~lines 180–195)
- Fix: Pass the variable through sudo explicitly: `sudo env DEBIAN_FRONTEND=noninteractive apt-get dist-upgrade ...` (or `sudo DEBIAN_FRONTEND=noninteractive apt-get ...`). Apply the same pattern anywhere env vars are set on a `sudo` invocation. Add a regression test asserting argv contains `env`/`VAR=value` rather than relying on `Command::env`. Also consider wrapping the spawned upgrade task in `tokio::time::timeout` so a hung dpkg prompt can't block forever.
- Owner: AI agent — mechanical change with a clear testable contract.

### 1.2 SIGHUP reload silently widens access when `allowed_clients` is removed
- Type: Security — fail-open configuration reload
- File: `daemon/src/client_auth.rs` (`reload_allowed_clients`, ~lines 145–156)
- Fix: Distinguish "key absent" from "key present but empty" (config already does this via `deserialize_present_list`). On reload, if the new config yields `None`, keep the existing non-empty allow-list and log an `error!` saying the removal was ignored; only honor `None` if the previous state was also `None`. Alternative: require an explicit sentinel (e.g. `allowed_clients: "*"`) to mean "any CA client". Update the config template comment documenting reload semantics.
- Owner: Human engineer — policy decision (fail-closed vs. documented fail-open) affecting the revocation workflow; an agent can implement once the policy is chosen.

### 1.3 GET `/status` performs privileged mutations
- Type: Security — side effects on a read endpoint / least-privilege violation
- Files: `daemon/src/main.rs` (`status_handler`, ~line 439), `daemon/src/package_manager/apt.rs` (`get_updates` → `apt-get update`, ~lines 100–123), `daemon/src/package_manager/brew.rs`
- Fix: Split cache refresh from status reporting. Options: (a) `/status` returns the cached update list only, and a new `POST /status/refresh` (operator role) triggers `apt-get update`; or (b) move cache refresh to a background tokio task on the TTL timer so no request path runs sudo. Option (b) preserves the API. Either way, `get_updates` should not invoke `sudo` inside a GET handler.
- Owner: Human engineer for the API-shape decision; AI agent for implementation once chosen.

### 1.4 Credential migration can lose secrets
- Type: Bug — data-loss ordering
- File: `cli/src/ca.rs` (`migrate_account`, ~lines 182–193)
- Fix: Reorder to `store.set(account, &value)` first, then `legacy.delete(account)` only after the set succeeds. If delete fails after a successful set, report `Migrated` with a warning rather than `Failed`. Update `test_migrate_all_from_legacy` and add a test where `set` fails and the legacy value must survive.
- Owner: AI agent — small, well-specified ordering fix with clear test expectations.

## Priority 2 — Medium (correctness bugs)

### 2.1 Port check-then-bind TOCTOU
- Type: Bug — race condition
- File: `daemon/src/main.rs` (~lines 275–302)
- Fix: Stop probing with a throwaway `TcpListener`. Attempt the real bind (via `axum_server::bind` or a listener kept and passed to `axum_server::from_tcp`), and on `AddrInUse` either fail (when `server.port` was explicitly configured — silent port drift breaks clients) or increment only in auto-discovery mode. The chosen port must be the one actually bound and the one advertised via mDNS.
- Owner: AI agent — keep auto-increment only for the `port: None` path.

### 2.2 `update_interval_mins * 60` overflow
- Type: Bug — arithmetic overflow
- Files: `daemon/src/package_manager/apt.rs` (line ~31), `daemon/src/package_manager/brew.rs` (line ~25)
- Fix: Use `update_interval_mins.checked_mul(60).map(Duration::from_secs).unwrap_or(Duration::MAX)` or clamp in `validate_config` (e.g. error if `> u64::MAX/60` or a sane max like 1 year). Prefer validating in `config.rs` so both backends are covered once.
- Owner: AI agent.

### 2.3 Reboot endpoint reports success on macOS while commands can't exist
- Type: Bug — platform mismatch + misleading response
- File: `daemon/src/main.rs` (`execute_reboot` ~618–643, `reboot_handler` ~649–659)
- Fix: (a) Gate reboot on `cfg!(target_os = "linux")` or a capability probe (`systemctl`/`reboot` exist), not on `pm_available`. (b) Return `PRECONDITION_FAILED`/`NOT_IMPLEMENTED` when the platform can't reboot instead of "reboot triggered". (c) Optionally add macOS support via `sudo -n /sbin/shutdown -r now`.
- Owner: AI agent for implementation; a human should confirm the intended platform matrix.

### 2.4 `apt-get -s dist-upgrade` counts new installs as updates
- Type: Bug — incorrect parsing
- File: `daemon/src/package_manager/apt.rs` (~lines 148–153)
- Fix: `Inst` lines for upgrades look like `Inst pkg [old] (new ...)`; new installs lack the `[...]` bracket. Filter to lines containing `[` before the version, or use `apt list --upgradable`. Add a fixture test with both an upgrade line and a new-install line.
- Owner: AI agent.

### 2.5 Dry-run bypasses the upgrade lock
- Type: Bug — missing mutual exclusion
- File: `daemon/src/main.rs` (`full_upgrade_handler`, ~lines 492–513)
- Fix: Run the dry-run under the same reboot-state check, and either reject with `LOCKED`/`PRECONDITION_FAILED` when `is_upgrading` is set, or take a shared "apt activity" lock so dry-run and upgrade serialize. Don't set `is_upgrading` for a dry-run, but do check it.
- Owner: AI agent.

### 2.6 `TimeoutLayer` returns 408 while work continues
- Type: Bug — cancellation semantics
- Files: `daemon/src/main.rs` (~354–358), `daemon/src/package_manager/apt.rs` (`run_async`/`spawn_blocking`, ~41–43)
- Fix: Document that timeouts don't cancel `spawn_blocking` work, and mitigate: (a) run external commands via `tokio::process::Command` instead of `spawn_blocking` + `std::process`, so dropping the future kills the child (`kill_on_drop(true)`); (b) ensure `last_update` is only set on genuine success.
- Owner: AI agent for the `tokio::process` migration; human review of the diff since it touches every command path.

## Priority 3 — Low (hygiene, hardening)

### 3.1 `$EDITOR` split on whitespace breaks paths with spaces
- Type: Hygiene — parsing. File: `daemon/src/main.rs` (~118–126)
- Fix: Use `shell-words`/`shlex` for proper quoting, or treat `EDITOR` as a single program path when it contains no whitespace-separated args.
- Owner: AI agent.

### 3.2 Only first CA cert loaded from bundle
- Type: Hygiene — robustness. File: `daemon/src/main.rs` (`load_tls_config`, ~858–864)
- Fix: Add all certs from the CA PEM to `RootCertStore` (the CLI's `root_store` in `cli/src/pin.rs` ~235–242 already does this — mirror it).
- Owner: AI agent.

### 3.3 Poisoned `upgrade_handle` mutex silently ignored
- Type: Hygiene — error handling. File: `daemon/src/main.rs` (~553–555)
- Fix: Use `lock().unwrap_or_else(|p| p.into_inner())` (pattern already used in `apt.rs` ~line 121).
- Owner: AI agent.

### 3.4 mDNS instance-name collisions on shared first DNS label
- Type: Hygiene — naming. File: `daemon/src/main.rs` (~764–766)
- Fix: Use the full hostname (dots replaced with `-`) or append a short hash of the full hostname when it differs from the first label.
- Owner: AI agent.

### 3.5 Homebrew refuses to run as root
- Type: Hygiene — operational check. File: `daemon/src/package_manager/brew.rs` (`is_available`, ~88–90)
- Fix: On macOS, have `is_available` verify the daemon isn't running as uid 0 (or that `brew` actually executes — check exit status, not just `Ok`), so `pm_available` reflects reality.
- Owner: AI agent.

### 3.6 Legacy keyring hex heuristic may mis-decode values
- Type: Hygiene — edge case. File: `cli/src/ca.rs` (`is_hex_encoding`/`read_legacy`, ~57–74)
- Fix: A stored value that happens to be ≥40 even-length hex chars gets silently hex-decoded into garbage. Only attempt decode when the decoded bytes are valid UTF-8 and the re-encoded hex round-trips, or store a format marker. Low priority — only affects legacy migration.
- Owner: AI agent.

## Verified non-issues (no action needed)
- EKU/cert-type confusion: `generate_leaf` in `cli/src/ca.rs` (~444–454) correctly separates `ServerAuth`/`ClientAuth`; `WebPkiClientVerifier` enforces clientAuth.
- `server.port: 0`: rejected by `validate_config` in `daemon/src/config.rs` (~359–363).
- Reboot/upgrade mutual exclusion and reboot-cancel ID check: correctly serialized (`daemon/src/main.rs` ~515–541).
- TOFU pinning: the probe verifies the cert against the CA via `WebPkiServerVerifier` before learning the fingerprint; mismatch requires explicit `--repin` (`cli/src/pin.rs` ~45–61).
- Key-file permissions: `validate_config` already warns on permissive daemon-key modes (`daemon/src/config.rs` ~460–473).

## Suggested execution order
1. Batch A (agent, one PR): 1.4, 2.2, 2.4, 2.5, 3.2, 3.3 — small, unambiguous, test-covered fixes.
2. Batch B (agent, one PR): 1.1, 2.1, 2.6 — command-execution and binding changes; needs `cargo test` + manual verification on a Linux node.
3. Batch C (human decision → agent implements): 1.2, 1.3, 2.3 — require policy/API decisions first.
4. Batch D (agent, opportunistic): 3.1, 3.4, 3.5, 3.6 — hygiene, bundle with related work.

Every fix should ship with a regression test; the codebase already has good test harnesses (`MockRunner`, `MemoryStore`, `run_tls_server`) to build on.
