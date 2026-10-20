# Standalone rustc test justification comment conciseness and avoidance of rule-mechanics/GN-comparison boilerplate

- **Learned from:** human review on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 4 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The `rustc_test_missing_standalone_comment` remediation in
`checks/bazel_minimality.sh` previously instructed the coding agent to state
both that unit tests need a `testutils` dependency and that `Bazel
rustc_test(crate = ...) inherits all deps from crate, unlike GN non_test_deps`,
causing the coder to copy that second clause verbatim into `BUILD.bazel` where
general rule-inheritance mechanics and `non_test_deps` comparisons are
redundant.

## Why not a one-off fix

Any `rustc_library_with_features` or `rustc_library` migration where
`non_test_deps` swaps a dependency for a `testutils` variant requires a
standalone `rustc_test` with a justifying comment; tightening both the
`rustc_test_missing_standalone_comment` remediation and adding
`rustc_test_verbose_standalone_comment` in `checks/bazel_minimality.sh` prevents
redundant rule-mechanics/GN-comparison clauses across all future Rust test
migrations.
