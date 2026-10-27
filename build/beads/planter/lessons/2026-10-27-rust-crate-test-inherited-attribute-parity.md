# Rust crate test inherited attribute parity

- **Learned from:** inner loop friction on CL Iaa2e4e4a06b03bca4b85c475637090a69a5c8251, patchset 1 (task `src-connectivity-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

In both rules_rust and bazel2gn, a rustc_test or host_rustc_test target that
sets crate = ":<lib>" automatically inherits edition, crate_root (source_root),
and rustc_env (rustenv) from the referenced crate target in the same BUILD.bazel
file, and bazel2gn inlines those inherited attributes into the generated GN
rustc_test target below the BAZEL2GN SENTINEL. However, diffengine (review_diff)
inspects only the test target's own keyword arguments in BUILD.bazel and flags
edition (or source_root/rustenv) in GN as not carried over to Bazel when the
test target relies on crate inheritance instead of repeating the attribute, and
gn_attr_parity excluded edition, source_root, and rustenv from its
crate-inherited attribute alignment INFO coverage.

## Why not a one-off fix

Suppressing the warning only for specific connectivity library directories would
leave every other dual-build or batch-migrated Rust package whose rustc_test or
host_rustc_test target inherits edition, source_root, or rustenv via crate =
":<lib>" vulnerable to the same false-positive review_diff attribute_alignment
warning.
