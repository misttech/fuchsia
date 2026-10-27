# Speculative test targets: migration invents Rust unit tests/test packages absent from the GN build

- **Learned from:** human review on CL I1862615828a5aa2bfe0bc8807e5ab7d517f5ca49, patchset 2 (task `src-recovery-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/manifest.json`, `checks/speculative_unit_test.sh`

## Root cause

Existing checks (cq_reachability, migration_sanity rust_unit_test_layout) push
every Rust library toward a packaged rustc_test and only verify that tests are
wired and packaged; nothing compared the new Bazel tests against whether the
base GN target had with_unit_tests or whether the crate sources contain any test
functions, so empty test binaries and packages passed.

## Why not a one-off fix

Deleting the empty test in one package leaves the coder free to add the same
empty rustc_test plus fx_unittest_package to any other test-less Rust library it
migrates; only a deterministic check against the base GN and crate sources
prevents it everywhere.
