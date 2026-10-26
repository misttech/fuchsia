# Migrated tests aggregate into ancestor test_suites rather than leaf bazel_test_suite stubs

- **Learned from:** ~60 reviewer comments across 44 CLs from Jay Zhuang, David Dorwin,
  Bruno Dal Bo, and Yen-Pu Wang (CL 1886281, CL 1913272, CL 1881999, CL 1872326)
- **Date:** 2026-10-26
- **Changed:** `prompts/coder.md`

## Root cause

Planter's coder prompt previously instructed agents to declare a local
`bazel_test_suite("bazel_tests")` in the leaf package's `BUILD.gn`. This had two
systematic failure modes:

1. It prevented complete removal of `BUILD.gn` (Case 1 migration) even when all
   library, binary, and test targets had fully migrated to Bazel, leaving behind
   an empty `BUILD.gn` whose sole target was a single-test `bazel_test_suite`.
2. It fragmented Fuchsia's test topology into hundreds of tiny test suites
   instead of aggregating into canonical, flat area-level suites (e.g.
   `//src/connectivity/wlan:target_tests`, `//tools:host_tests`,
   `//src/sys:target_tests`, `//sdk/lib:target_tests`, `//src/storage:target_tests`).

Reviewers repeatedly asked authors to delete the leaf `bazel_test_suite`, wire the
test into the ancestor `test_suite`, set `visibility = ["//<ancestor>:__pkg__"]`,
and delete `BUILD.gn` completely.

## Remediation

`prompts/coder.md` now establishes a clear priority order:

1. Coder agents first inspect ancestor directories for existing flat test suites
   (`test_suite(name = "target_tests"|"host_tests")`).
2. When found, the migrated test is wired directly into the ancestor suite's
   `tests = [...]` list, and `visibility = ["//<ancestor>:__pkg__"]` is declared.
3. If all package targets have migrated, `BUILD.gn` is deleted entirely (Case 1).
4. Local `bazel_test_suite` bridges in leaf `BUILD.gn` files are used only as a
   fallback when no ancestor test suite exists in the hierarchy.
