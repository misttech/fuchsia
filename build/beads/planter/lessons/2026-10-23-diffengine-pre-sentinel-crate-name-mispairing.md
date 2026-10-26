# Diffengine pre sentinel crate name mispairing

- **Learned from:** inner loop friction on CL I35d897ef924a3b16c57d1eac675f6850d03fd072, patchset 2 (task `swd-pkg-multi-universal-tool`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`

## Root cause

When an unmigrated GN target (such as a kept `fuchsia_package("<crate_name>")`)
stays above the `BAZEL2GN SENTINEL` in `BUILD.gn` and no Bazel target has `name
= "<crate_name>"`, `diffengine` greedily matches the pre-sentinel GN target to a
differently named Bazel target (such as `rustc_library(name = "lib", crate_name
= "<crate_name>")`) via `crate_name`/`output_name`/`binary_name` before reaching
the same-named GN target generated below the sentinel, producing false-positive
`dep_alignment`, `dropped_comment`, and `attribute_alignment` warnings on the
Bazel target.

## Why not a one-off fix

Hardcoding exemptions for individual package directories would fail to prevent
identical `diffengine` greedy `crate_name` mis-pairings in any other dual-build
Fuchsia package where a pre-sentinel GN package or binary target shares its name
with a sibling library's `crate_name`.
