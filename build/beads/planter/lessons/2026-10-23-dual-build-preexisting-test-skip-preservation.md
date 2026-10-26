# Dual build preexisting test skip preservation

- **Learned from:** inner loop friction on a local task (task `src_sys_time_httpsdate_time_source_tests`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh parsed base_target_skips and base_attr_skips at
$PLANTER_CHANGE_BASE to exempt pre-existing code-owner test skips from the
commented-skip check (skip_records), but failed to consult base_target_skips and
base_attr_skips in the subsequent bare-skip checks (and in
is_untranslatable_skipped_bazel_test), causing false-positive
unjustified_test_skip errors whenever a package with a pre-existing bare #
@bazel2gn:skip on a test target or test attribute was touched (e.g., to grant
visibility).

## Why not a one-off fix

Exempting only a single package path or target name would leave every other
dual-build BUILD.bazel file with pre-existing bare # @bazel2gn:skip annotations
on test targets or test attributes vulnerable to the same false-positive
unjustified_test_skip error whenever modified for visibility or incremental
migration.
