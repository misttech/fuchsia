# Ancestor area flat test suite wiring

- **Learned from:** human review on CL If6a81435bf8531155538049cfca7fc9444b5835f, patchset 4 (task `src_sys_time_timekeeper_persistence`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

nearest_ancestor_test_suite in checks/cq_reachability.sh only detected
pre-existing ancestor BUILD.bazel test_suite targets already exported by an
ancestor bazel_test_suite, and prompts/coder.md instructed adding a per-package
bazel_test_suite in the leaf BUILD.gn whenever no ancestor test_suite for that
parameter existed yet.

## Why not a one-off fix

Fixing only a single leaf package leaves every future migration that introduces
the first target_tests or host_tests Bazel test under an area directory without
a pre-existing BUILD.bazel test_suite vulnerable to adding a per-package
bazel_test_suite in the leaf BUILD.gn instead of bootstrapping the flat area
test_suite.
