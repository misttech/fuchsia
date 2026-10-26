# Rust with unit tests gn decomposition attribute alignment

- **Learned from:** inner loop friction on a local task (task `pkg-cache-bin`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

check_rust_target_attr_alignment_info in checks/gn_attr_parity.sh gated the 'GN
with_unit_tests decomposed into explicit test target' attribute_alignment INFO
note solely on has_bazel_test_rule (a test rule in BUILD.bazel). When a migrated
Rust library or binary decomposes with_unit_tests = true into an explicit
rustc_test('<name>_test') target in BUILD.gn because its test package remains in
BUILD.gn (due to a GN-only dependency), has_bazel_test_rule is false, so
unit_test_opts omitted the decomposition note and review_diff emitted an
uncovered attribute_alignment WARNING.

## Why not a one-off fix

Any dual-build Rust library or binary whose unit test package remains in
BUILD.gn due to unmigrated or GN-only test dependencies must decompose
with_unit_tests = true into an explicit rustc_test target in BUILD.gn when the
production target moves to BUILD.bazel, so checking both BUILD.bazel and
BUILD.gn for the explicit test target is required across all future Rust
migrations.
