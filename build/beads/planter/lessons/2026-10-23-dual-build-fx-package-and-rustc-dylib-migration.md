# Dual build fx package and rustc dylib migration completeness

- **Learned from:** human review on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 9 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

The migration_sanity check still treated transitive rustc_dylib dependencies as
a blocker for Bazel packaging and only audited unmigrated GN fuchsia_package
targets above the sentinel when BUILD.bazel already declared an fx_package,
allowing convertible assembly fuchsia_package targets whose components were
already migrated to BUILD.bazel to remain in BUILD.gn.

## Why not a one-off fix

Fixing only a single package directory leaves the obsolete rustc_dylib packaging
exemption and the missing pre-sentinel fuchsia_package audit in
migration_sanity.sh, which would allow future migrations of packages or tests
depending on shared Rust libraries to leave convertible fuchsia_package or
fuchsia_unittest_package targets in BUILD.gn.
