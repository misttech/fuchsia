# Dual build rustc test consumed by kept gn test package

- **Learned from:** inner loop friction on a local task (task `pkg-tests-pkgctl`)
- **Date:** 2026-10-20
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`, `prompts/coder.md`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (test skips only)

## Status: partly superseded

The `test_generated_by_bazel2gn` exemption and the `# @bazel2gn:skip`
requirement on `rustc_test` targets that this lesson worked around no longer
exist: bazel2gn now generates GN copies of tests, and tests of dual-built
targets run in both build systems by default
(docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems).
The `check_unskipped_dual_build_test_targets` changes were dropped when this
lesson was rebased onto that policy. Packaging and `tests.json` reachability
guidance below still applies.

## Root cause

While migration_sanity already computed gn_kept_pkg_test_deps to exempt
dual-built test targets consumed by a blocked GN test package above the BAZEL2GN
SENTINEL from test_generated_by_bazel2gn, the second AST pass in
migration_sanity (rust_unit_test_layout), cq_reachability
(unpackaged_rust_device_test), and coder.md still unconditionally required #
@bazel2gn:skip and a Bazel fx_test/fx_unittest_package on every rustc_test
target and lacked INFO coverage for review_diff.

## Why not a one-off fix

Any integration test package whose fuchsia_test_package must remain in BUILD.gn
above the sentinel due to GN-only component dependencies or rustc_dylib linkages
will hit the exact same false-positive rust_unit_test_layout,
unpackaged_rust_device_test, two_case_rule, and attribute_alignment errors when
its rustc_test binary is migrated to BUILD.bazel and synced via fx bazel2gn.
