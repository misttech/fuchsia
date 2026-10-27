# Test fixture component rustc test packaging

- **Learned from:** inner loop friction on a local task (task `src-sys-run-test-suite-directory-and-fixtures`)
- **Date:** 2026-10-26
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

The rust_unit_test_layout check in checks/migration_sanity.sh flagged any
non-host rustc_test target whenever its BUILD.bazel file lacked a top-level
fx_unittest_package or fx_test rule, without checking whether the rustc_test
binary is wrapped into an fx_component test fixture target (synced to GN
fuchsia_component via bazel2gn and packaged by another directory's integration
test package) or consumed by a GN test package kept above the sentinel.

## Why not a one-off fix

Test fixture directories and multi-package integration test suites across the
tree frequently define standalone rustc_test binaries wrapped in
fx_packaged_binary and fx_component (synced to fuchsia_component via bazel2gn)
that are packaged into an ancestor or sibling integration test package rather
than exposed as standalone top-level test packages; checking fx_component and
kept GN test package references in migration_sanity.sh prevents false-positive
rust_unit_test_layout errors across all such packages.
