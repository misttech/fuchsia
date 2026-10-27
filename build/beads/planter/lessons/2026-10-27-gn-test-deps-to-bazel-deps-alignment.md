# Gn test deps to bazel deps alignment

- **Learned from:** inner loop friction on a local task (task `src-power-manager-configs-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

In GN, standalone test templates such as rustc_test accept test_deps (and
deps_for_tests) and merge them into deps inside the template implementation,
whereas Bazel test rules such as rustc_test only expose deps. Because diffengine
(review_diff) only extracts deps, public_deps, link_deps, non_rust_deps, and
proc_macro_deps from the base BUILD.gn target, any dependency originally
declared in GN test_deps or deps_for_tests and migrated to Bazel deps is flagged
as an unmatched Bazel dependency in review_diff dep_alignment, and
gn_attr_parity did not account for base GN test_deps/deps_for_tests when
recording INFO dep_alignment coverage.

## Why not a one-off fix

Hardcoding target names or package paths would only silence the warning for a
single test target, leaving all future migrations of standalone GN test targets
that list dependencies under test_deps or deps_for_tests blocked by
false-positive review_diff dep_alignment warnings. Recording INFO dep_alignment
coverage in gn_attr_parity whenever unmatched Bazel dependencies match base GN
test_deps or deps_for_tests resolves this class of false positive across all
packages.
