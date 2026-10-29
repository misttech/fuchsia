# Incremental visibility list rollup scoping

- **Learned from:** human review on CL I6c67c92e89847a6089233b3a242d32d0da29000f, patchset 2 (task `netemul-test-support`)
- **Date:** 2026-10-26
- **Changed:** `checks/visibility_audit.sh`

## Root cause

visibility_audit.sh gated the overly_narrow_global_visibility check on
len(new_entries) > 15 instead of checking whether the target's visibility was
modified (bool(new_entries)) and evaluating total len(entries). Consequently,
shared targets in pre-existing BUILD.bazel files (such as cml_shard targets)
whose visibility lists grew by one leaf :__pkg__ entry per migration CL were
never flagged to roll up into subsystem or top-level subpackage wildcards.

## Why not a one-off fix

Updating only a single shared BUILD.bazel target leaves visibility_audit.sh
blind to every other shared library, FIDL target, or cml_shard whose visibility
list is extended incrementally one caller at a time across migrations.
