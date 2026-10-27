# Dual build bazel2gn test rdep visibility recognition

- **Learned from:** inner loop friction on CL Ic5bb4e10a944dd20a41c157911fdb6208c7aad15, patchset 3 (task `src-developer-ffx-lib-diagnostics-analytics`)
- **Date:** 2026-10-27
- **Changed:** `checks/visibility_audit.sh`

## Root cause

When a dual-build package defines a test target (such as host_rustc_test or
rustc_test) that bazel2gn translates into BUILD.gn below the BAZEL2GN SENTINEL,
compute_gn_nonconvertible_targets in visibility_audit.sh could still classify
the target name as GN-only or non-synced if a pre-sentinel target (such as a
local bazel_test_suite) or non-sentinel check matched it, causing ancestor
BUILD.gn relative dependency references (e.g. '<subdir>:<test>') to be filtered
out via is_gn_only_target_ref and triggering false_positive_rdep_visibility when
granting :__pkg__ visibility to the ancestor package.

## Why not a one-off fix

Any dual-build library or test package whose bazel2gn-generated GN targets below
BAZEL2GN SENTINEL are referenced by relative labels from an ancestor BUILD.gn
group('tests') requires Bazel visibility for that ancestor package because
bazel2gn propagates Bazel visibility into the generated GN target; inspecting
the generated section below the sentinel in visibility_audit.sh fixes this
across all packages.
