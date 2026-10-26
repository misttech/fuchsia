# Preexisting collateral bazel package and target isolation

- **Learned from:** inner loop friction on [CL 1837927](https://fuchsia-review.googlesource.com/c/fuchsia/+/1837927), patchset 3 (task `src_developer_ffx_lib_doctor_utils`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `checks/visibility_audit.sh`

## Root cause

When a migration touches a pre-existing ancestor or collateral BUILD.bazel
solely to register a newly migrated test in test_suite(...) or update visibility
without migrating that ancestor package's BUILD.gn, visibility_audit flagged
pre-existing package(default_visibility) and unscoped exports_files()
declarations from $PLANTER_CHANGE_BASE (and bypassed change_base exemption
whenever PLANTER_TARGET_DIR was iterated over collateral directories), while
review_diff flagged pre-existing package_hygiene, sentinel_drift,
missing_sources, dep_alignment, and dropped_comment findings on untouched
targets in the collateral package.

## Why not a one-off fix

Exempting only a single ancestor directory would fail whenever any other library
migration registers a unit test in an ancestor test_suite() or updates
visibility in a partially migrated parent BUILD.bazel that contains pre-existing
targets, default_visibility, or GN-only constructs.
