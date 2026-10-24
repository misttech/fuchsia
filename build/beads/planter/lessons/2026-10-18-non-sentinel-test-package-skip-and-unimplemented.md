# Non sentinel test package skip and unimplemented fidl rust next blocker

- **Learned from:** inner loop friction on a local task (task `src_lib_fdomain_example_next`)
- **Date:** 2026-10-18
- **Changed:** `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh unconditionally required `# @bazel2gn:skip` on every
`rustc_test` in `rust_unit_test_layout` even when `BUILD.gn` had no `## BAZEL2GN
SENTINEL` (e.g. test-only packages keeping only `bazel_test_suite` and
`group("tests")`), directly contradicting `review_diff/two_case_rule` which
forbids `# @bazel2gn:skip` in non-sentinel packages. Additionally,
`migration_sanity.sh` did not detect when a target depends on `*_rust_next` or
`*_rust_fdomain_next` FIDL bindings while `enable_rust_next` in
`//build/bazel/rules/fidl/fidl_library.bzl` is an unimplemented stub, leaving
the coder with an unbuildable `BUILD.bazel` in the working tree.

## Why not a one-off fix

Any test-only Rust package whose `BUILD.gn` retains only `bazel_test_suite` and
`group("tests")` without `## BAZEL2GN SENTINEL` hits the same check
contradiction between `rust_unit_test_layout` and `two_case_rule`, and any Rust
library or test depending on `*_rust_next` or `*_rust_fdomain_next` FIDL
bindings will fail Bazel analysis until `fidl_library.bzl` implements
`enable_rust_next`.
