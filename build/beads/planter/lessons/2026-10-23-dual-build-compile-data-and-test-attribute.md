# Dual build compile data and test attribute alignment parity

- **Learned from:** inner loop friction on CL I582bbb30f3a509e83fce5bad216965e8dd0463d3, patchset 1 (task `swd-pkg-system-updater`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`

## Root cause

Planter's built-in review_diff deduplicates findings against INFO compatibility
findings emitted by checks in checks/manifest.json rather than run_checks.sh's
local downgrade filter. However, migration_sanity.sh only inspected targets
still present in the current BUILD.gn (missing GN test targets deleted from
BUILD.gn when migrated to Bazel) and only emitted raw variable names for GN
inputs rather than top-level GN variable-expanded paths, while gn_attr_parity.sh
and migration_sanity.sh omitted the 'GN output_name=... handled implicitly by
Bazel target/package rule' and 'rustenv (...) != rustc_env (...)' clause
combinations.

## Why not a one-off fix

Any GN target (especially migrated rustc_test or Case 1 full-removal targets)
that carried top-level variable-referenced compile-time inputs (mapped to Bazel
compile_data/data) or combined GN output_name with rustenv (mapped to Bazel
rustc_env and fx_packaged_binary) will trigger identical review_diff
missing_sources and attribute_alignment false positives unless both
migration_sanity.sh and gn_attr_parity.sh emit matching INFO compatibility
findings across base and current BUILD.gn targets.
