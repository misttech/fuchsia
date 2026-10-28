# Bazel-only host tests wired via an ancestor bazel test suite

- **Learned from:** human review on CL I0ca6b534019534464132bf6d840128f01434d787, patchset 2 (task `src-intl-strings-to-fidl`)
- **Date:** 2026-10-28
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When a host tool is built in Bazel only and bridged to GN with
`bazel_host_tool()` (instead of being synced to GN by bazel2gn), its package
`BUILD.gn` intentionally no longer defines a GN test target or
`group("tests")`. The host test is instead listed in an ancestor Bazel
`test_suite` (for example `//src/intl:host_tests`) that a wired GN
`bazel_test_suite("bazel_tests")` exports. `dropped_tests_group` only accepted
GN-side wiring (it strips `bazel_test_suite(...)` blocks before searching
ancestors), so it reported a false positive and asked the coder to restore a
GN `group("tests")`, contradicting the reviewer.

## Fix

`dropped_tests_group` is now suppressed when nothing references
`//<dir>:tests` and every Bazel test target of the package is reached through
a `bazel_test_suite` that `suite_is_wired()` accepts, following Bazel
`test_suite` aggregation in ancestor and local packages.

## Why not a one-off fix

Any host tool migrated with `bazel_host_tool()` and an aggregated ancestor
host test suite hits the same false positive.
