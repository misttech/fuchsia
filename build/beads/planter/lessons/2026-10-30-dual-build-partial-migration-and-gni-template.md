# Dual build partial migration and gni template visibility analysis

- **Learned from:** inner loop friction on CL Ia429928fe0ec59c2ebe63d7763cc167c91b27617, patchset 6 (task `src_lib_dso_cpp`)
- **Date:** 2026-10-30
- **Changed:** `checks/migration_sanity.sh`

## Root cause

First, migration_sanity.sh checked whether an external dependency's BUILD.bazel
existed or matched a literal name attribute without distinguishing packages that
only export non-library files (such as exports_files for .ifs stubs) or
partially migrated packages from packages that define the target directly or via
macro-generated sub-targets (such as fidl_library language bindings), causing
convertible GN targets blocked on unmigrated dependencies in partially migrated
packages to be misclassified as duplicate_target_above_sentinel or causing
migrated FIDL sub-targets to be misclassified as unmigrated dependencies.
Second, visibility_audit.sh and find_rdeps.sh only attributed reverse
dependencies in a package's BUILD.gn or BUILD.bazel, missing packages that only
define .gni templates which inject dependencies into downstream callers at
template expansion time.

## Why not a one-off fix

Hardcoding exemptions for a single library target or a single .gni template
package would leave all other partially migratable packages (where one target in
BUILD.gn is blocked on an unmigrated dependency whose package only has
exports_files or partial Bazel targets) and all other .gni template wrapper
packages vulnerable to false-positive duplicate_target_above_sentinel and
false_positive_rdep_visibility errors across future Fuchsia GN-to-Bazel
migrations.
