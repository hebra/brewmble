# Security Hardening Implementation Plan

**Date:** 2026-10-23
**Source:** Security analysis of 2026-10-23 (all 14 findings, ordered by severity)
**Scope:** `cli/`, `daemon/`, `rest/`, `daemon/Containerfile`, `daemon/Makefile`, `docs/`, `.github/workflows/`, `README.md`, `AGENTS.md`

## Decisions (resolved with user)

- **Node identity:** TOFU fingerprint pinning in the CLI. Store each node's leaf-cert SHA-256 fingerprint in `~/.brewmble.yaml` on first contact; pin on subsequent connects; hard-fail with a clear message on change (with an explicit re-pin escape hatch).
- **Scope:** All 14 findings from the analysis.

## Execution order & task list

Tasks are grouped into independently reviewable PRs, ordered by risk-reduction per effort. Within each PR, tasks are sequential.

---

### PR 1 — Quick wins (low-risk, high-value)

**T1. Sanitize mDNS-derived strings** (finding #2)
- Add `fn sanitize_display(s: &str) -> String` in `cli/src/main.rs` (or a small new `cli/src/sanitize.rs`): strip C0 controls (except none — node names should be single-line), C1 controls, ESC (0x1B), BEL (0x07), DEL (0x7F).
- Apply in `entry_id()`, `entry_host()`, `entry_instance()`, `entry_tls()` and to node `name` values at `load_config()` time (defense-in-depth for already-poisoned configs).
- Unit tests: escape sequences, OSC 52 payload, unicode preserved, idempotence.

**T2. `is_upgrading` panic guard** (finding #4)
- In `daemon/src/main.rs` `full_upgrade_handler`: wrap flag reset in a drop guard (`struct UpgradingGuard(Arc<AtomicBool>)` with `Drop` impl) created inside the spawned task before `full_upgrade().await`.
- Test: mock PM that panics in `full_upgrade`; assert flag is false after task join.

**T3. Reboot hardening** (finding #5)
- `daemon/src/main.rs` `reboot_handler` spawned task:
  - Use `sudo -n` for both `systemctl reboot` and `reboot` fallback.
  - Run fallback when the first command's `ExitStatus` is non-zero (not only on spawn error).
  - Implement `RebootRequest.delay_secs`: `tokio::time::sleep(Duration::from_secs(d))` before issuing the reboot when `Some(d)`; keep `None` = immediate.
- Tests: existing handler tests keep passing; add test that `delay_secs` is accepted (verify via mock/seam — extract the reboot command execution into a testable fn taking a `CommandRunner`-like abstraction or a function pointer; keep it simple: extract `async fn execute_reboot(delay: Option<u64>)` and unit-test the decision logic with an injectable runner).

**T4. Poisoned-mutex tolerance** (finding #14a)
- `daemon/src/package_manager/apt.rs` and `brew.rs`: replace `.lock().unwrap()` with `.lock().unwrap_or_else(|p| p.into_inner())` for `last_update`.

**T5. Bare-IPv6 `resolve_url`** (finding #14c)
- `cli/src/main.rs` `resolve_url()`: first try `target.parse::<std::net::IpAddr>()`; if `Ok(IpAddr::V6)` → `https://[{addr}]`. Keep existing host:port logic otherwise.
- Extend `test_resolve_url` with `::1` → `https://[::1]`.

**T6. EDITOR with arguments** (finding #14b)
- `cli/src/main.rs` (`ProfileCommands::Edit`) and `daemon/src/main.rs` (`run_config_edit`): split `$EDITOR` with a minimal whitespace split (no new dependency; document that quoting is unsupported) — first token is the program, rest are prepended args before the file path.

**T7. Atomic, private config writes** (finding #14d)
- `cli/src/main.rs` `save_config()`: write to `<path>.tmp`, set `0600` via `OpenOptionsExt::mode` on unix, then `fs::rename`. Also apply to `daemon/src/config.rs` `save_config()`/`init_config_file()` (0600 for daemon config since it references key paths).
- Non-unix: plain write + rename (no mode).

---

### PR 2 — Daemon robustness (async/privilege)

**T8. Move subprocesses off tokio workers** (finding #3)
- `daemon/src/package_manager/mod.rs`: keep the sync `CommandRunner` trait; add `async fn run_async(runner: &dyn CommandRunner, program: &'static str, args: &'static [&'static str])` — or simpler: in `apt.rs`/`brew.rs`, wrap each `self.runner.run(...)` call site in `tokio::task::spawn_blocking(move || ...)` (requires `Arc<dyn CommandRunner>` instead of `Box`; change `runner` field type accordingly in `Apt`, `Brew`, and constructors/tests).
- Also wrap `version()`/`is_available()` call sites used from async contexts (they're called in handlers). Simplest: cache both at startup (see T9) so async paths never call them.
- Existing `MockRunner` tests stay valid (trait unchanged).

**T9. Cache `version()`/`is_available()` at startup** (finding #3 companion, from previous audit P2)
- `daemon/src/main.rs`: compute `pm.is_available()` and `pm.version()` once in `main()`, store in `AppState`; handlers read cached values. `status_handler`'s per-request `is_available()` check uses the cached value.

**T10. HTTP resilience** (finding #9)
- Add `tower-http` `timeout` feature; apply `TimeoutLayer::new(Duration::from_secs(300))` to the router in `daemon/src/main.rs`.
- Track the upgrade `JoinHandle` in `AppState` (`Arc<Mutex<Option<JoinHandle<()>>>>`); in the shutdown path, abort-or-await it before exit (await with a bounded timeout, then abort).
- apt commands: set env `DEBIAN_FRONTEND=noninteractive` and add `-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold` to `full-upgrade` invocation in `apt.rs`. `RealCommandRunner::run` needs an env-injection mechanism — simplest: add `fn run_with_env(&self, program, args, env: &[(String,String)])` with a default impl ignoring env for mocks, or build the `Command` in `Apt` directly. Decide during implementation; prefer adding env support to the trait with a default method.

---

### PR 3 — TOFU fingerprint pinning (finding #1)

**T11. Fingerprint pinning in CLI**
- `cli/src/main.rs` `NodeConfig`: add `#[serde(default, skip_serializing_if = "Option::is_none")] fingerprint: Option<String>`.
- `cli/src/ca.rs`: add `fn peer_fingerprint(client: &reqwest::blocking::Client, url: &str) -> Result<String, Box<dyn Error>>` — perform a GET to `PATH_HEALTH`, extract `resp.extensions()` TLS peer identity… **practical approach with reqwest blocking:** reqwest does not expose peer certs directly. Simplest robust approach: use `rustls` + `TcpStream` directly in a small `pin.rs` module: connect, complete handshake with a custom `ServerCertVerifier` that accepts the cluster CA (reuse existing root) and captures the leaf DER, compute SHA-256 fingerprint (format matches `ca::fingerprint`: colon-hex).
  - Alternative (simpler, acceptable): reqwest `ClientBuilder::tls_built_in_root_certs(false)` + custom danger callback is not available in blocking API — so the dedicated rustls connector module is the way. Keep it small (~120 LOC).
- Pinning flow helper `fn check_pin(node: &NodeConfig, addr: &str) -> Result<PinOutcome, Box<dyn Error>>` in CLI:
  - `pin` absent → fetch fingerprint, return `Learned(fp)`; caller persists via `save_config` (merge into the matching node entry) and prints `Pinned <node> <fp>`.
  - `pin` present and matches → `Ok`.
  - mismatch → error: `fingerprint mismatch for <node>: expected <exp>, got <got>; run 'brewmble node --repin <node>' to accept the new identity`.
- Wire into `run_status`, `run_node`, `run_version` before issuing requests (fail that node with the mismatch message, continue others).
- Add `brewmble node --repin <name>` flag (conflicts with `-u/-r/-l/--dry-run`): clears the stored fingerprint, re-learns, saves.
- `merge_nodes()` (discover `-u`): when an existing node's **address changes**, clear its stored fingerprint (log/print that re-pinning is required) — prevents silent stale pins (old audit #12 analogue).
- Tests: unit-test fingerprint format/match logic with generated rcgen certs on a local rustls server (reuse the daemon test cert helpers pattern); test mismatch error message; test `--repin` clears and re-learns.
- Docs: update `README.md` Security section (TOFU model, `--repin`, threat note about stolen daemon keys still requiring the pinned address) and `AGENTS.md` patterns section.

---

### PR 4 — PKI hygiene & operational hardening

**T12. `brewmbled config validate` checks** (finding #6)
- `daemon/src/config.rs` `validate_config()`:
  - Parse leaf cert: check expiry (warn if <30 days, error if expired), EKU contains ServerAuth, and that cert public key matches the configured private key (compare via `rustls`/ring public-key derivation; if impractical with current deps, at minimum verify the key parses and the cert chain verifies against the CA — implement with `webpki` already pulled transitively by rustls, or shell the check through rustls `ServerConfig::with_single_cert` which fails on mismatch: reuse `load_tls_config` in validate and report its error).
  - Keep it pragmatic: calling the existing `load_tls_config()` from `validate_config` and surfacing its error covers pair-match and parse validity; add explicit expiry check by parsing the leaf with `x509-parser` (new small dep) — or skip x509-parser: rustls does not check expiry at config-build time, so expiry needs a parser. Accept `x509-parser` dependency in daemon.
- Tests: expired cert fixture (rcgen with past `not_after`), mismatched key, wrong EKU.

**T13. CA operational docs + issuance logging** (finding #7)
- `cli/src/main.rs` `run_cert`/`run_ca`: on every issuance, print CN + fingerprint + expiry to stdout (already partially done for CA init; extend to leaf issuance).
- `README.md`: document that the operator machine keyring is the cluster trust root; rotation playbook (issue new CA with `--force`, re-issue daemon + client certs, rolling restart); note the 397-day leaf / 10-year CA lifetimes.

**T14. Privilege-model hardening** (finding #8)
- `README.md` / `daemon/README.md`: document exact sudoers whitelist, e.g.:
  ```
  brewmble ALL=(root) NOPASSWD: /usr/bin/apt-get update, /usr/bin/apt-get -s dist-upgrade, /usr/bin/apt full-upgrade -y, /usr/bin/apt-get autoclean, /usr/bin/apt-get autoremove -y, /usr/bin/systemctl reboot, /sbin/reboot
  ```
- `daemon/src/package_manager/apt.rs` + `daemon/src/main.rs` reboot: invoke `/usr/bin/sudo` by absolute path (fallback to `sudo` from PATH if the absolute path is missing — try absolute first). Keep package-manager binaries as-is (sudo resolves them via secure_path).
- `docs/brewmbled.service.sample`: add hardening directives compatible with the sudo requirement: `NoNewPrivileges=false` (must stay false for sudo), `ProtectSystem=full`, `ProtectHome=read-only`, `PrivateTmp=true`, `ProtectKernelTunables=true`, `ProtectControlGroups=true`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK`, `CapabilityBoundingSet=` (empty), plus a comment explaining the sudoers requirement.
- `docs/com.github.hebra.brewmble.brewmbled.plist.sample`: change `StandardOutPath`/`StandardErrorPath` to `~/Library/Logs/brewmble/brewmbled.{out,err}` (user-owned).

---

### PR 5 — Low-severity platform & dependency items

**T15. Container image** (finding #10)
- `daemon/Containerfile`: remove `&& apt full-upgrade -s`; add `RUN useradd -r -s /usr/sbin/nologin brewmble` + `USER brewmble` (note: apt operations inside the container will fail without sudo — document that the container image is for brew-less/apt-host-mounted scenarios or add sudo; simplest: keep root but add a prominent comment + README note that container deployments should drop capabilities; decide at implementation: default to non-root + document limitation).
- `daemon/Makefile`: add comment that mDNS requires `--network host` (Linux) or reflector; keep `EXPOSE 5353` but note UDP.

**T16. Bind-address option** (finding #11)
- `daemon/src/config.rs` `ServerConfig`: add `#[serde(default)] bind_address: Option<IpAddr>` (default `0.0.0.0` to preserve behavior).
- `daemon/src/main.rs`: use it for both the port-probe bind and the server bind; include in config template with comment; validate (no special rules needed).
- Tests: deserialization default, explicit loopback.

**T17. Gate CWD config pickup** (finding #12)
- `cli/src/main.rs` `resolve_config_path()`: only use `./.brewmble.yaml` when `--config`/`BREWMBLE_CONFIG` is not set **and** print a notice `using config from current directory: <path>` to stderr; do not auto-create in CWD (already only falls back to home for creation). Update `test_resolve_config_path` accordingly.

**T18. Dependency hygiene** (finding #13)
- Add `cargo audit` job to a new `.github/workflows/audit.yml` (uses `rustsec/audit-check` or `cargo install cargo-audit`; run on PR + weekly schedule).
- Remove `home` crate from both `cli/Cargo.toml` and `daemon/Cargo.toml`; replace with `std::env::home_dir()` (Rust ≥1.85; repo uses edition 2024 — verify MSRV in CI).
- Migrate `serde_yaml` → `serde_yaml_ng` in `cli` and `daemon` (API-compatible; update imports).
- Bump `keyring` 2.3.2 → 3.x in `cli/Cargo.toml`; adapt `Entry::new`/`get_password`/`set_password` API changes; re-run keyring tests.

**T19. Discovery timeout + misc leftovers from prior audit**
- `cli/src/main.rs` `run_status` discovery thread: use `get_default_timeout()` instead of hardcoded 5s (finding P8).
- Dedupe `initial_targets` by address only (P10).

---

## Cross-cutting requirements

- **Tests:** every task ships unit tests in the existing style (mock `CommandRunner`, in-memory `CredentialStore`, oneshot routers). Target ≥90% coverage on new code per `AGENTS.md`.
- **Docs:** `README.md` Security section updated in PR 3 and PR 4; `AGENTS.md` updated where patterns change (TOFU pinning, spawn_blocking runner, bind_address, sudoers requirement).
- **Commits:** Conventional Commits (`fix(daemon): ...`, `feat(cli): add TOFU fingerprint pinning`, etc.).
- **Backwards compatibility:** 
  - `NodeConfig.fingerprint` is optional — old configs load fine; first contact after upgrade learns pins.
  - `bind_address` defaults to `0.0.0.0` — no behavior change.
  - `delay_secs` becomes honored — API contract clarification, not a break.

## Failure modes & risks

- **TOFU lockout:** operator re-issues a daemon cert legitimately → CLI hard-fails with mismatch; mitigation is the documented `--repin` flow (T11).
- **spawn_blocking + `Arc<dyn CommandRunner>`:** touches many test constructors in `apt.rs`/`brew.rs`; mechanical change, verify with `cargo test`.
- **keyring 3.x migration:** API surface changed; isolate in one commit; if migration balloons, split into its own PR.
- **Container non-root:** apt upgrade inside container will fail; resolved by explicit documentation decision in T15.
- **x509-parser dependency (T12):** adds a native-free pure-Rust dep; acceptable.

## Validation plan

1. `cargo test` green in `cli/`, `daemon/`, `rest/` after each PR.
2. `cargo build --release` for both binaries; run `brewmbled config validate` against fixtures (valid, expired, mismatched key).
3. Manual end-to-end on macOS host: `brewmble ca init --force`, `cert issue` both kinds, start `brewmbled` with generated certs, `brewmble status`, `node --list`, `node --full-upgrade --dry-run`, verify pin written to `~/.brewmble.yaml`, tamper pin → verify mismatch error, `--repin` → success.
4. `cargo audit` clean (or documented exceptions).
5. Container build: `cd daemon && make container` succeeds.

## Open questions (non-blocking)

- T10: exact env-passing mechanism for `CommandRunner` (default method vs. building `Command` in `Apt`) — decide in implementation.
- T15: non-root container vs. documented root — decide in implementation; recommendation is non-root + documented limitation.
