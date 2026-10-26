# Unbuilt gn target preexisting bitrot repair

- **Learned from:** inner loop friction on a local task (task `src_sys_time_time_test_client`)
- **Date:** 2026-10-22
- **Changed:** `checks/build_only_scope.sh`, `checks/build_verification.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

When a migrated GN package was disconnected from the configured GN build graph
(<build dir>/ninja_outputs.json has no active targets for the package at
$PLANTER_CHANGE_BASE), its source files were never compiled by GN in CQ and may
contain pre-existing hard compile errors (syntax, type, or removed-API bitrot
from upstream migrations) that no BUILD.bazel attribute can resolve. However,
build_only_scope.sh unconditionally rejected all source-file edits as errors and
prompts/coder.md instructed the coder never to edit sources and to stop if
parity was impossible without a source change, creating an unresolvable conflict
with build_verification.

## Why not a one-off fix

Allowing source edits only for one specific package path would fail whenever any
other GN package that was disconnected from the configured GN build graph (or
only referenced by an unconfigured canonical group) is migrated to Bazel and
fails bazel_build_fuchsia or bazel_build_host due to accumulated upstream API or
syntax bitrot.
