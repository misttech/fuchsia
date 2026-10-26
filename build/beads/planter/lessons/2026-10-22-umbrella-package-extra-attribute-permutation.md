# Umbrella package extra attribute permutation coverage

- **Learned from:** inner loop friction on a local task (task `swd-pkg-null-update-checker`)
- **Date:** 2026-10-22
- **Changed:** `checks/migration_sanity.sh`

## Root cause

When a migration updates a shared non-bazel2gn umbrella package such as
//bundles/assembly/BUILD.bazel alongside an existing BUILD.gn, Planter's
diffengine compares pre-existing umbrella targets (such as bringup and platform)
against their GN bazel_build_action counterparts and emits attribute_alignment
warnings whose semicolon-separated clauses combine deterministic known
attributes (such as testonly) with randomized Go map iteration over ExtraAttrs.
In checks/migration_sanity.sh, record_excluded_dir_gn_unmodified_info only
expanded clause permutations when a message had at most 3 clauses, so targets
with 4 or 7 clauses only recorded the single ordering from one subprocess
invocation and missed the ordering produced by the parent process's review_diff
check.

## Why not a one-off fix

Suppressing only a single target or hardcoding a single clause ordering would
fail on subsequent runs or other AIB package migrations because Go map iteration
randomizes the order of the ExtraAttrs suffix on every execution.
