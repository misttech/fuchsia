# Test support visibility scoping allows tree-wide subpackages visibility

- **Learned from:** reviewer friction and machinery conflict on task `pkg-hyper-test-support`
- **Date:** 2026-10-15
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

In `checks/visibility_audit.sh`, test support utilities (`testonly = True` libraries under `src/testing/` or `src/lib/testing/`) and test entry points (`host_rustc_test`, `wrap_host_*`, `fx_test`, etc.) were evaluated under strict production visibility scoping rules:
1. `visibility = ["//visibility:public"]` was unconditionally rejected outside `//build/bazel_sdk/...` as `overbroad_public_visibility`.
2. `visibility = ["//:__subpackages__"]` was rejected as `disguised_public_visibility` unless the target met `is_globally_used_target` (requiring >= 6 distinct areas and >= 20 packages).

For test support libraries (such as `//src/lib/testing/fuchsia-hyper-test-support`) that originally had default public visibility in GN (`"*"`), human reviewers frequently request unrestricted visibility so future tests across any subsystem can reuse test fixtures without having to edit visibility lists. However, when the migration agent tried to grant broad visibility per reviewer feedback, `visibility_audit.sh` rejected both `//visibility:public` and `//:__subpackages__`, causing an unresolvable machinery conflict.

## Remediation

1. Exempted test entry point rules (`host_rustc_test`, `host_test`, `wrap_host_*`, `fx_test`, `fx_unittest_package`, `fx_test_package`) from `overbroad_public_visibility`.
2. Allowed `//:__subpackages__` (and top-level/umbrella wildcards) for `testonly = True` targets and packages located under `src/testing/` or `src/lib/testing/` without triggering `disguised_public_visibility` or `overbroad_top_level_subpackages_visibility`.
3. Updated `recommend_visibility` in `checks/visibility_audit.sh` and `tools/find_rdeps.sh` to recommend `["//:__subpackages__"]` for test-only targets consumed across multiple areas.
