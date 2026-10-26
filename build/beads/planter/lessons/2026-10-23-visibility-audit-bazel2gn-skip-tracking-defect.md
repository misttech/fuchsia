# Visibility audit bazel2gn skip tracking defect

- **Learned from:** inner loop friction on CL I4eb1208833ce04e0f6df1ff341fbc538e588d2cc, patchset 9 (task `pkg-http-client`)
- **Date:** 2026-10-23
- **Changed:** `checks/visibility_audit.sh`

## Root cause

In checks/visibility_audit.sh, the per-file AST pass referenced
bazel2gn_skipped_targets and src_lines when determining whether a BUILD.bazel
target is synced to BUILD.gn by bazel2gn (is_gn_synced_target and
@bazel2gn:raw_overwrite inspection), but failed to initialize src_lines and
bazel2gn_skipped_targets or populate bazel2gn_skipped_targets via
has_bazel2gn_skip_comment(src_lines, node.lineno), causing a NameError whenever
visibility_audit inspected any target with a visibility attribute.

## Why not a one-off fix

This crash occurs in the shared visibility_audit check script whenever any
migrated package defines a BUILD.bazel target with an explicit visibility list;
initializing src_lines and populating bazel2gn_skipped_targets in
checks/visibility_audit.sh fixes the check across all Fuchsia GN-to-Bazel
migration tasks.
