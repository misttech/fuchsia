# Dual build crate name mispairing alignment

- **Learned from:** inner loop friction on CL I35d897ef924a3b16c57d1eac675f6850d03fd072, patchset 2 (task `swd-pkg-multi-universal-tool`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When an unmigrated pre-sentinel GN packaging target shares its name with the
crate_name, output_name, or binary_name of a differently named Bazel library or
binary target whose GN definition is generated below the BAZEL2GN SENTINEL,
diffengine greedily pairs the pre-sentinel GN target with the Bazel target and
emits false-positive dep_alignment, dropped_comment, and attribute_alignment
warnings when review_diff runs standalone or when the pre-sentinel GN target
carries its own deps or output/binary name attributes.

## Why not a one-off fix

Any dual-build Fuchsia package that retains a hand-written GN package or
component target above the BAZEL2GN SENTINEL while defining a library or binary
target (such as lib or bin) with crate_name, output_name, or binary_name
matching the package name will trigger the same greedy target mis-pairing in
diffengine unless both migration_sanity INFO coverage and run_checks.sh
review_diff downgrading handle all resulting dep_alignment, dropped_comment, and
attribute_alignment permutations generically.
