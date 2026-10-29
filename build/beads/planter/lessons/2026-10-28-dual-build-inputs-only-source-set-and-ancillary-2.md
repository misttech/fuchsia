# Dual build inputs only source set and ancillary filegroup parity

- **Learned from:** inner loop friction on a local task (task `ffx-product-leaves-b`)
- **Date:** 2026-10-28
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

migration_sanity previously treated every GN source_set above the BAZEL2GN
SENTINEL as a convertible C/C++ library and flagged any same-named #
@bazel2gn:skip target in BUILD.bazel when in_pre_gn was true, even when the GN
source_set only wrapped non-code inputs files for a Bazel filegroup (which
bazel2gn does not translate) or already existed before the change in an
ancillary package touched only for exports_files or visibility.

## Why not a one-off fix

Hardcoding a single SDK metadata directory or target name would fail whenever
another dual-build package pairs an inputs-only GN source_set with a #
@bazel2gn:skip Bazel filegroup or when a migration edits an ancillary
sentinel-synced package that retains pre-existing skipped or pre-sentinel
targets.
