# Preexisting upstream verify bazel2gn shac drift

- **Learned from:** cq failure on CL If6a81435bf8531155538049cfca7fc9444b5835f, patchset 2 (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-22
- **Changed:** `checks/build_verification.sh`

## Root cause

The shac verify_bazel2gn check scans all sentinel BUILD.gn files across the
repository rather than only files touched by the change. When an unrelated
package on main has pre-existing BUILD.gn/BUILD.bazel drift, fx host-tool shac
check in build_verification fails even though the migration change did not
modify that package, unlike gn_build which already downgrades pre-existing
failures outside the changed packages to INFO.

## Why not a one-off fix

Running bazel2gn on the unrelated package inside this migration CL would pollute
the change with out-of-scope edits rejected by bazel_minimality. Teaching
build_verification.sh to classify shac_check failures whose reported paths are
all outside the changed packages as INFO preexisting_failure_shac_check prevents
false-positive deadlocks across all future migration tasks whenever upstream
main has verify_bazel2gn drift.
