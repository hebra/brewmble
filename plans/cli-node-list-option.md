# CLI Subcommand `node`: Add `list` Option

## Goal
Add a `--list` / `-l` option to the `brewmble node` (and `brewmble packages` alias) subcommand that lists the names of the nodes configured in the active (current) profile.

## Key Changes
1. **CLI Parser (`cli/src/main.rs`)**:
   - Add `list: bool` field to `Commands::Node` with `#[arg(short = 'l', long, conflicts_with_all = ["full_upgrade", "reboot", "dry_run"])]`.
   - Update argument documentation for `list` to describe listing node names configured in the active profile.
2. **Command Handler (`run_node`)**:
   - Update `run_node` signature to accept `list: bool`.
   - Update validation to require at least one action among `--full-upgrade`, `--reboot`, or `--list`.
   - Resolve target nodes using `resolve_profile_nodes(&nodes, config)`.
   - If `list` is true, iterate over resolved nodes and print each node's configured name (falling back to address if name is not set), without initializing TLS / keyring clients.
3. **Tests (`cli/src/main.rs`)**:
   - Update existing CLI parse tests to include the `list` field in `Commands::Node` pattern matches.
   - Add CLI parse unit tests for `brewmble node --list`, `brewmble node -l`, and `brewmble packages --list`.
   - Add unit tests for conflicts between `--list` and `--full-upgrade`, `--reboot`, and `--dry-run`.
   - Add unit tests validating `run_node` execution with `list = true` (e.g. empty nodes list, populated profile, fallback addresses).
4. **Documentation**:
   - Update `cli/README.md` and `README.md` with examples and description for `brewmble node --list` / `-l`.

## Verification
- Run all CLI tests with `cd cli && cargo test`.
- Verify manual test cases / edge cases.
