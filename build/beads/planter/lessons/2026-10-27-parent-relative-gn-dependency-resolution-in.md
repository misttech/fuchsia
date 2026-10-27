# Parent-relative GN dependency resolution in visibility auditing and deferred-directory handling in multi-package batch migrations

- **Learned from:** inner loop friction on CL I714afb1d66bf041eca70dc0b7af8841907fba02f, patchset 2 (task `src-power-manager-configs-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/migration_sanity.sh`, `checks/visibility_audit.sh`

## Root cause

visibility_audit.sh only searched for absolute '//' package labels and
child-relative labels from ancestor BUILD.gn files, missing sibling/cousin
BUILD.gn files that reference a target via '../' parent-relative labels, and
flagged enclosing depth>=3 subtree wildcards when only one caller in the subtree
existed. Additionally, when a multi-directory batch task left a subset of
blocked target directories unmigrated (no BUILD.bazel and unmodified BUILD.gn)
while migrating the remaining directories, review_diff reported gn_unmodified on
the unmigrated batch directories because migration_sanity.sh did not read
PLANTER_TARGET_DIRS or record INFO gn_unmodified coverage for deferred batch
directories.

## Why not a one-off fix

Exempting a single package or target directory would leave visibility_audit
blind to '../' relative GN dependency labels across all other subsystems and
would continue to fail with gn_unmodified whenever any multi-directory batch
migration defers a blocked directory in the batch.
