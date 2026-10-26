# Non bazel2gn and excluded package gn unmodified compat

- **Learned from:** inner loop friction on a local task (task `pkg-cache-config`)
- **Date:** 2026-10-20
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When a migration modifies a BUILD.bazel file that only exports shard files,
defines # @bazel2gn:skip targets, or resides in an umbrella/excluded directory
where a sibling BUILD.gn intentionally has no bazel2gn sentinel and is not
modified, review_diff emits a false-positive gn_unmodified ERROR unless
migration_sanity and run_checks record or downgrade the gn_unmodified finding to
INFO across all modified BUILD.bazel directories.

## Why not a one-off fix

Hardcoding a single shard or package directory would leave any future component,
structured-config, or test migration that touches exports_files(), #
@bazel2gn:skip targets, or shared build/assembly packages with an unmodified
sibling BUILD.gn vulnerable to the same gn_unmodified false positive.
