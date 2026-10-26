# Dual build package rdeps and gni template visibility

- **Learned from:** inner loop friction on a local task (task `swd-pkg-system-update-committer`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

migration_sanity unconditionally flagged any pre-sentinel GN fuchsia_package or
fuchsia_component as duplicate_gn_fuchsia_package whenever BUILD.bazel defined
fx_package, ignoring active external GN reverse dependencies on
non-bazel2gn-convertible packaging targets; visibility_audit and find_rdeps
ignored GN reverse dependencies introduced via imported .gni template expansions
when the caller package only named the target directory directly in its own
visibility allowlist; and migration_sanity/gn_attr_parity only recorded INFO
rule-mapping alignment for fuchsia_* templates mapped to fx_* rules, missing
*_binary GN wrapper templates mapped to fx_packaged_binary.

## Why not a one-off fix

Exempting a single package directory or hardcoding a specific binary template
would fail on every subsequent dual-build package migration where GN integration
tests or recovery groups still depend on the GN package/component targets or
where shared binary wrapper templates in .gni files expand to dependencies
evaluated in the caller package scope.
