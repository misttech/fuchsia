# Bazel dependency package reachability and stacked ancestry

- **Learned from:** cq failure on CL I7938a093b025d82103ee81db30fab26c7b4ba636, patchset 2 (task `src-developer-ffx-lib-mdns-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

When generating tests.json during CQ builds, bazel cquery loads the full
transitive closure of Bazel packages reachable from exported bazel_test_suite
targets. Previously, cq_reachability.sh only inspected direct dependency
attributes in the target directory's BUILD.bazel (and omitted attributes such as
lint_config, components, test_components, subpackages, and resources), missing
missing_bazel_dependency_package and stacked_bazel_dependency_package edges
reached transitively through newly migrated or stacked parent packages.

## Why not a one-off fix

Any migrated package whose exported Bazel tests transitively depend on an
unmigrated package or on a package introduced by an unmerged stacked parent
commit will trigger the same Bazel query 'no such package: BUILD file not found'
failure during CQ tests.json generation unless cq_reachability.sh verifies both
direct and transitive Bazel dependency packages across all label-bearing
attributes.
