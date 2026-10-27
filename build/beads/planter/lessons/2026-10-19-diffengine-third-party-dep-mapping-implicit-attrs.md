# Diffengine third party dep mapping implicit attrs and exports only gn unmodified

- **Learned from:** inner loop friction on a local task (task `pkg-far-cpp-tests`)
- **Date:** 2026-10-19
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

Planter's built-in diffengine (review_diff) compares raw GN and Bazel target
ASTs without consulting //build/tools/bazel2gn/third_party_target_map.json,
flags redundant default output_name and workspace-root include_dirs omitted on
Bazel C/C++ binary/test targets as attribute_alignment warnings, and flags
gn_unmodified errors when a helper package's BUILD.bazel only exports files
(exports_files) without bazel2gn-convertible targets. Furthermore,
migration_sanity.sh only ran its diffengine INFO deduplication pass when pre-
and post-sentinel target templates overlapped and did not pass -d with all
modified candidate directories.

## Why not a one-off fix

Suppressing these findings only for a single package directory would leave every
future migration that maps third-party dependencies via
third_party_target_map.json, migrates C/C++ test executables with default
output_name or workspace-relative include_dirs, or exports .cml shards from an
unmigrated helper package via exports_files() blocked on identical review_diff
false positives.
