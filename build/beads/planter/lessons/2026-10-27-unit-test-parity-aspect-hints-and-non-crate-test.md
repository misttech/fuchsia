# Unit test parity aspect hints and non crate test deps coverage

- **Learned from:** inner loop friction on a local task (task `src-storage-benchmarks`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/visibility_audit.sh`, `tools/run_checks.sh`

## Root cause

When a GN Rust or C++ library with `with_unit_tests = true` declares `test_deps`
containing an `expect_includes()` target (or non-crate dependency carried via
`aspect_hints`, `data`, `test_data`, `compile_data`, `link_deps`,
`third_party_target_map.json`, or `@bazel2gn` directives), the decomposed
`rustc_test` target in `BUILD.bazel` must place the `expect_includes()` target
in `aspect_hints` rather than `deps` (because `rustc_test.deps` requires
`CrateInfo` and `bazel2gn` translates `aspect_hints` back into GN `deps +=
[...]`). However, `review_diff` (`diffengine`) only checks `deps` and
`proc_macro_deps` for `unit_test_parity`, and neither `checks/gn_attr_parity.sh`
nor `tools/run_checks.sh` emitted or recognized `INFO` `unit_test_parity`
coverage when all unmatched `test_deps` were carried via `aspect_hints` or other
valid Bazel unit-test attributes.

## Why not a one-off fix

Hardcoding an exception for a single package or `expect_includes` target would
leave every future migration that decomposes `with_unit_tests = true` with
`expect_includes()`, `data`/`compile_data`, `link_deps`, or `@bazel2gn`
directives in `test_deps` vulnerable to the same false-positive
`review_diff/unit_test_parity` error.
