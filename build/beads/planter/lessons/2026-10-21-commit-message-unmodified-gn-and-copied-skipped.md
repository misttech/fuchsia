# Commit message unmodified gn and copied skipped test targets

- **Learned from:** human review on CL I6383cb2795407c205b78c8f935cbf3bcfd2b815e, patchset 2 (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-21
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

check_body_unmigrated_targets in checks/commit_message_format.sh skipped targets
in kept_gn_by_dir whenever their names appeared in migrated_targets (which
includes # @bazel2gn:skip rustc_test, go_test, and fx_test_component targets for
Bazel-Migration-Target footer purposes) and did not check whether <dir>/BUILD.gn
was modified at all, allowing the commit body to claim 'Migrate <test_target>
... and sync BUILD.gn via fx bazel2gn' when the GN target remained hand-written
above the sentinel and BUILD.gn was untouched; additionally,
commit_subject_missing_tag required adding a [<subsystem>] tag even when doing
so pushed '[bazel_migration][<subsystem>] //<dir>' past the 65-character subject
limit, causing the coder to truncate the subject path to an unmodified parent
directory.

## Why not a one-off fix

Fixing only the single package's commit message leaves
checks/commit_message_format.sh blind to future test-only or partial dual-build
changes where # @bazel2gn:skip targets are copied to BUILD.bazel while BUILD.gn
is untouched, or where long package paths exceed 65 characters when combined
with a subsystem tag.
