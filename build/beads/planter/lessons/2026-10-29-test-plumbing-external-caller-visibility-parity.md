# Test plumbing external caller visibility parity

- **Learned from:** inner loop friction on a local task (task `b4-extr1`)
- **Date:** 2026-10-29
- **Changed:** `checks/migration_sanity.sh`

## Root cause

The rust_test_target_hygiene check in checks/migration_sanity.sh unconditionally
treated all fx_test_component, fx_unittest_component, rustc_test, testonly
fx_packaged_binary/fx_component_manifest, and test fx_package targets as purely
internal package-private plumbing and rejected any visibility attribute on them,
even when external packages depend on the target and find_rdeps/target_parity
require declaring caller-scoped visibility.

## Why not a one-off fix

Exempting a single target name or directory would leave the same contradiction
between migration_sanity and find_rdeps/visibility_audit whenever any shared
test component, manifest, or binary in another package is referenced by external
integration test packages.
