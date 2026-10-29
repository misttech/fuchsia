# Dual build inputs only source set and ancillary sentinel packages

- **Learned from:** inner loop friction on a local task (task `ffx-product-leaves-a`)
- **Date:** 2026-10-28
- **Changed:** `checks/build_verification.sh`, `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

migration_sanity.sh treated every GN source_set() above BAZEL2GN SENTINEL as a
convertible C/C++ library and flagged any same-named # @bazel2gn:skip target in
BUILD.bazel via in_pre_gn, even when the Bazel target is a filegroup (which
bazel2gn does not support in bazelRuleToGNTemplate) paired with an inputs-only
GN source_set(inputs = ...) that groups non-code files for non_rust_deps, or
when an already-migrated ancillary package outside target_dirs is touched only
for exports_files or visibility.

## Why not a one-off fix

Exempting a single package directory would leave every other dual-build package
that pairs a # @bazel2gn:skip Bazel filegroup with an inputs-only GN source_set,
or any already-migrated sentinel package touched to export files or widen
visibility, vulnerable to false-positive skipped_convertible_target and
duplicate_target_above_sentinel errors.
