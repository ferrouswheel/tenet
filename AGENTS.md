# Agent Instructions

## Repository Overview
Tenet is a Rust reference implementation of a peer-to-peer social network protocol concept. It includes:
- Core library and binaries in `src/` for the CLI, relay service, debugger UI, and simulation tools.
- Documentation in `docs/` covering architecture, MVP scope, and simulation guidance.
- Scenario fixtures in `scenarios/` used by the simulation harness.
- Tests in `tests/` and integration tests for the simulation harness.

If the repository structure, purpose, or key components change substantially, update this overview to match.

## Required Pre-Completion Checklist
Before finishing any change, always run these steps to prevent breakages:
1. Format the codebase: `cargo fmt`
2. Ensure the project builds: `cargo build`
3. Run the test suite: `cargo test`

These checks must be completed for every change before you finalize your work.
