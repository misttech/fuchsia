# Gn bazel dep label slash canonicalization

- **Learned from:** inner loop friction on a local task (task `src_lib_inet`)
- **Date:** 2026-10-20
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

GN permits non-canonical dependency labels with redundant leading or internal
slashes (such as four leading slashes), whereas Bazel requires canonical
double-slash package paths and rejects extra slashes unless overwritten with a
directive that triggers bazel2gn_overwrite/bazel2gn_directive warnings. While
gn_dep_parity already stripped redundant slashes when comparing dependency sets,
gn_attr_parity's check_dep_alignment_info and run_checks.sh did not normalize
redundant slashes when checking whether unmatched GN and Bazel dependency labels
represent the same canonical target, leaving review_diff's dep_alignment warning
unsuppressed.

## Why not a one-off fix

Hardcoding an exception for a single target or dependency path would fail
whenever any other BUILD.gn file in the tree contains a dependency label with
accidental extra leading or internal slashes that must be normalized to a
canonical Bazel label during migration. Normalizing redundant slashes generally
in gn_attr_parity's dep_alignment coverage check and run_checks.sh handles all
such non-canonical GN labels across the repository.
