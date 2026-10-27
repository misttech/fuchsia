# Preexisting dependency bazel target minimality exemption

- **Learned from:** inner loop friction on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 3 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-27
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

When a migration modifies a pre-existing BUILD.bazel in a dependency package
(for example, widening visibility on a cml_shard or library) or in a partially
migrated package, bazel_minimality.sh includes that BUILD.bazel in bazel_files
and previously ran check_rustc_test_raw_output_name,
check_rustc_test_crate_reuse, redundant default attribute checks, and
path_overwrite placeholder checks on all targets in the file without checking
whether the target and attribute/annotation already existed at
$PLANTER_CHANGE_BASE. Demanding that the coder rename or refactor pre-existing
targets directly contradicts migration_sanity/dropped_preexisting_bazel_target
and triggers unrelated diff review warnings.

## Why not a one-off fix

Exempting a single dependency package or target name would fail whenever another
migration widens visibility or adds a cml_shard in any other already-migrated
BUILD.bazel containing pre-existing rustc_test targets with
@bazel2gn:raw_attribute:output_name or legacy attributes. Indexing pre-existing
(rule, name) target AST slices at $PLANTER_CHANGE_BASE in bazel_minimality.sh
systemically prevents false-positive findings on untouched pre-existing targets
across all packages while still enforcing minimality on newly added targets and
attributes.
