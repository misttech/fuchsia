# Concurrent build and worktree check serialization

- **Learned from:** inner loop friction on a local task (task `pkg-tests-omaha-client`)
- **Date:** 2026-10-29
- **Changed:** `checks/build_verification.sh`, `checks/tests_json_parity.sh`, `tools/run_checks.sh`

## Root cause

While the missing find_planter_bin definition in gn_attr_parity.sh was already
resolved in the current machinery snapshot, tools/run_checks.sh submitted all
enabled checks—including build_verification and tests_json_parity—into a
single ThreadPoolExecutor simultaneously when builds were enabled, and neither
check held a per-workdir mutual-exclusion lock. Consequently, tests_json_parity
temporarily reverted working-tree BUILD.gn/BUILD.bazel files, modified args.gn,
and invoked fx gen (regenerator.py) concurrently while static checks read build
files and build_verification ran fx build (regenerator.py) and computed
working-tree reuse fingerprints.

## Why not a one-off fix

Re-running build_verification and tests_json_parity manually one by one for a
single package migration does not prevent the race: every migration task
executes a full run_checks.sh pass without --skip-build before reporting, so
parallelizing workspace-mutating fx gen runs alongside static checks and fx
build causes systemic regenerator.py collisions, transient false-positive file
reads, and broken build-cache fingerprints across all future GN-to-Bazel
migrations.
