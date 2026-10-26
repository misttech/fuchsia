# Ancestor bazel test suite aggregation

- **Learned from:** human review on CL Ie6b56850fbf6188d1b473d5af21940c00ac92203, patchset 2 (task `src_lib_watch_handler`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

checks/cq_reachability.sh verified that every migrated Bazel test (fx_test,
fx_unittest_package, or host_*_test) was listed in SOME GN bazel_test_suite, and
its unexported_bazel_test remediation instructed the coder to add a per-package
bazel_test_suite in the leaf BUILD.gn. When an ancestor directory already
defines a BUILD.bazel test_suite (target_tests or host_tests) exported by a
wired ancestor bazel_test_suite, cq_reachability.sh accepted a per-package
bazel_test_suite in the leaf BUILD.gn instead of requiring the test to be wired
into the nearest ancestor BUILD.bazel test_suite.

## Why not a one-off fix

Hardcoding a single library path or suite would leave all other subdirectories
under areas with aggregate ancestor BUILD.bazel test_suite targets (such as
//src/lib, //src/storage, //src/diagnostics, //src/connectivity, //src/starnix,
//tools, and //sdk) vulnerable to adding per-package bazel_test_suite wrappers
in leaf BUILD.gn files instead of registering migrated tests in the nearest
ancestor BUILD.bazel test_suite.
