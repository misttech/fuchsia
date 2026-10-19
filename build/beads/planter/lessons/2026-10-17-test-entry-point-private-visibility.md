# Test entry points are package-private, never `//visibility:public`

- **Learned from:** human review of CL 1893881 (task `pkg-mock-health-verification-tests`)
- **Date:** 2026-10-17
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`

## Root cause

The coder put `visibility = ["//visibility:public"]` on
`fx_unittest_package(name = "mock-health-verification-tests")`. The coder
prompt's `fx_unittest_package` example has no `visibility`, but the coder
copied the pattern from already-migrated siblings in `//src/sys/pkg/testing/`
(`mock-boot-arguments`, `mock-installer`, `mock-metrics`), which all paired a
private `rustc_test` with a public `fx_unittest_package`.

No check caught it. The 2026-10-15 test-support visibility lesson exempted every
test rule (`is_test_rule`: `fx_unittest_package`, `fx_test`, `fx_test_package`,
`host_*_test`, `*_test`) from `overbroad_public_visibility` in
`checks/visibility_audit.sh`. That lesson was about shared test-support
*libraries* that other tests depend on. Test entry points have no Bazel
dependents. The GN `bazel_test_suite()` bridge lists them in
`bazel_host_test_suites.txt` / `bazel_target_test_suites_to_build.txt`, and
`//build/bazel/host_tests` and `//build/bazel/target_tests` build those as
top-level Bazel targets (`extra_bazel_targets_file`). `fx test` also builds them
as top-level targets. Bazel only checks visibility on dependency edges, so
these targets need no visibility at all.

## Remediation

1. `visibility_audit.sh` no longer exempts test rules from
   `overbroad_public_visibility`. For a test rule, the remediation is to omit
   `visibility` when no Bazel package depends on it. When a Bazel `test_suite()`
   in another package aggregates it, the remediation lists only those packages
   (for example `["//<parent>:__pkg__"]`).
2. `visibility_audit.sh` reports `//:__subpackages__` on a test rule with no
   Bazel dependents as `overbroad_test_target_visibility`, so the coder can't
   swap one tree-wide visibility for another.
3. `prompts/coder.md` states that test entry points are package-private and
   says not to copy public visibility from siblings.

The shared test-support library rules from the 2026-10-15 lesson are unchanged.
