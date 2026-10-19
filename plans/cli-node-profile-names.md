# CLI Subcommand `node`: Use Profile Node Names

## Goal
Transition the `brewmble node` (and `brewmble packages` alias) subcommand from expecting raw `host:port` targets to resolving node names configured in the active CLI profile.

## Key Changes
1. **CLI Parser (`cli/src/main.rs`)**:
   - Update `Commands::Node` positional argument from `targets` to `nodes`.
   - Update argument documentation to clarify that node names from the active profile are expected.
2. **Node Resolution (`resolve_profile_nodes`)**:
   - Extract helper function to resolve nodes from the active profile.
   - If no node names are provided on the CLI, default to all nodes in the active profile.
   - If specific node names are provided, look up each name against `node.name` in the active profile (or fallback to address for unnamed nodes).
   - If any requested node name is not found in the active profile, return a clear, descriptive error indicating the missing node and profile.
3. **Execution & Table Formatting (`run_node`)**:
   - Format output table with `NODE\tSTATUS` header using the resolved node name (or address fallback).
   - Direct HTTP requests to the resolved `node.address`.
4. **Tests (`cli/src/main.rs`)**:
   - Update existing CLI parse tests to check the `nodes` argument.
   - Add comprehensive unit tests for `resolve_profile_nodes` covering all execution paths and edge cases.
5. **Documentation**:
   - Update `cli/README.md`, `README.md`, and `ARCHITECTURE.md` to reflect that node names (rather than targets) are passed to `node`.

## Verification
- Run all CLI tests with `cargo test`.
- Verify formatting and error messages.
