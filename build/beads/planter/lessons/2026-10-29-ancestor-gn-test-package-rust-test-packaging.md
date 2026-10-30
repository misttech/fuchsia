# Recognize dual-built rustc_test binaries packaged by ancestor BUILD.gn test packages

- **Date:** 2026-10-29
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`

## Root cause

`is_packaged_by_kept_gn_test_package` in `checks/cq_reachability.sh` and
`check_rust_unit_test_layout` in `checks/migration_sanity.sh` only checked the
same directory's `BUILD.gn` for pre-sentinel `fuchsia_test_package` or
`fuchsia_unittest_package` targets. Integration `rustc_test` sub-directories
whose `BUILD.gn` defines only the test binary while an ancestor `BUILD.gn` wires
`<sub_dir>:<test_bin>` into a multi-component `fuchsia_test_package` were falsely
flagged as unpackaged standalone unit tests.

## Why not a one-off fix

Multi-component integration test suites under `tests/<subtest>/BUILD.gn`
routinely package child `rustc_test` binaries in the parent `tests/BUILD.gn`
alongside realm components that fail at runtime if run in an isolated
`fx_unittest_package`.
