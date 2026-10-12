# Commit message bazel migration target footer scoping

- **Learned from:** human review on CL I5e6909ad02268b0e3db900ff8d5d521b1779fa8d, patchset 2 (task `json_merge`)
- **Date:** 2026-10-12
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

The initial commit was authored before commit_message_format enforced
Bazel-Migration-Target: footers, and commit_message_format's target discovery
listed all non-helper targets in BUILD.bazel without filtering out targets that
already existed in BUILD.bazel at $PLANTER_CHANGE_BASE or synthetic binary =
":<bin>" wrapper targets referenced by host_test/fx_packaged_binary that did not
exist in the base BUILD.gn.

## Why not a one-off fix

Any partially migrated package or package using host_test/fx_packaged_binary
binary wrappers will have pre-existing BUILD.bazel targets or internal *_bin
test wrapper targets; checking base BUILD.bazel and base BUILD.gn in
commit_message_format.sh ensures accurate Bazel-Migration-Target: footer
recommendations and validation across all future migrations.
