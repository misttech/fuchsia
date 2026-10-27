# Unlanded in-tree Bazel package dependencies and CQ dry-run gating

- **Learned from:** Tricium CQ tryjob failure on CL 1917358 (task `src-developer-ffx-lib-target`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `fleet.go`, `planter.go`, `gerrit/client.go`

## Root Cause

1. **Pre-upload reachability blind spot:** When a BUILD.bazel target was migrated, its `deps` referenced an in-tree package (`//src/developer/ffx/lib/diagnostics/analytics:lib`) whose migration CL 1915678 had not yet landed on `origin/main`. Because the prerequisite was present locally in the checkout slot, local `run_checks.sh` and `fx build` succeeded.
2. **Standalone push onto origin/main:** `standaloneUploadCommit` pushed a commit parented directly on `origin/main`. On Gerrit tryjobs (Tricium), `gn gen` evaluated `bazel_test_suite` across all packages to populate `tests.json`. Because `src/developer/ffx/lib/diagnostics/analytics/BUILD.bazel` did not exist on `origin/main`, Bazel cquery failed with `ERROR: no such package 'src/developer/ffx/lib/diagnostics/analytics': BUILD file not found`.
3. **Ungated CQ dry runs:** Planter's monitor (`maybeTriggerCQDryRun`) and `planter run --cq` (`cqLoop`) voted `Commit-Queue+1` without verifying whether prerequisite tasks in `DependsOn` were landed on `origin/main`.
4. **Endless remediation loop:** When Tricium failed with `ERROR: no such package`, `fix-cq` treated it as a generic failure, attempting rebases and new patchsets rather than recognizing it as a blocked prerequisite package.

## Fix

1. **`checks/cq_reachability.sh`:** Added `check_unlanded_bazel_dependencies` to statically audit all `//...` in-tree labels in `BUILD.bazel`. When a dependency package does not have a `BUILD.bazel` on `origin/main` and the commit is parented directly on `origin/main`, `cq_reachability.sh` emits a blocking ERROR finding (`unlanded_bazel_dependency_package`).
2. **`fleet.go`:** Updated `maybeTriggerCQDryRun` to inspect `store.UnlandedDependencies(t)`. If prerequisite tasks in `DependsOn` have not landed on `origin/main`, CQ dry-run voting is skipped until they land.
3. **`planter.go`:** Updated `cqSkipReason`, `cmdRun`, and `cqLoop.run` to skip CQ dry runs when `task.WIP` is true or when prerequisite tasks have not landed.
4. **Intelligent CQ failure detection:** Added `MissingBazelPackage()` to `gerrit.CQChecksReport` and regex matching in `fleet.go` / `planter.go` to detect `no such package` in CQ logs, immediately pausing the task as WIP rather than retrying blindly.
