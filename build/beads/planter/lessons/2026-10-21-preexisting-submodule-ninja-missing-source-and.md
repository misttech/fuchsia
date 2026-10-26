# Preexisting submodule ninja missing source and bazel2gn rustc env compile data parity

- **Learned from:** inner loop friction on a local task (task `pkg-update-package-tests`)
- **Date:** 2026-10-21
- **Changed:** `checks/build_verification.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

unrelated_gn_ninja_failure in build_verification.sh rejected all outputs
containing 'ninja: error:', failing to recognize 'ninja: error: <src>, needed by
<obj>, missing and no known rule to make it' caused by an out-of-sync git
submodule or subrepository outside the changed packages and their reverse
dependencies. Additionally, migration_sanity.sh and run_checks.sh only inspected
pre-sentinel GN targets for review_diff compatibility and did not record INFO
coverage for Bazel compile_data covering GN inputs or bazel2gn-generated
rustc_env (@bazel2gn:raw_overwrite) and lint_config attributes below the
BAZEL2GN SENTINEL.

## Why not a one-off fix

Any migration in a worktree whose git submodules or subrepositories lag behind
origin/main after rebasing will hit Ninja missing-source errors in unrelated
packages during fx build, and any dual-build Rust library using compile_data,
rustc_env with @bazel2gn:raw_overwrite, or lint_config will trigger
false-positive review_diff missing_sources and attribute_alignment findings
unless handled generally across all packages.
