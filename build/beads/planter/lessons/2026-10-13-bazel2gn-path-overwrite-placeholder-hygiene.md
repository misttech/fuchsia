# Bazel2gn path overwrite placeholder hygiene

- **Learned from:** human review on CL If2ba29d6afa754cc8f0a5b0f91c004150f381a21, patchset 5 (task `dmlconfig`)
- **Date:** 2026-10-13
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

When an unbuilt select() branch in BUILD.bazel needed #
@bazel2gn:path_overwrite: for GN-only dependencies lacking Bazel targets, no
static check prevented using an arbitrary unrelated real Bazel target as a
stand-in to satisfy genquery or required consolidating multiple unmigrated
GN-only labels onto a single documented # @bazel2gn:skip placeholder filegroup.

## Why not a one-off fix

Fixing only the single package's BUILD.bazel leaves future dual-build migrations
with platform-conditional GN-only dependencies free to pick arbitrary unrelated
real Bazel targets or duplicate placeholder targets for #
@bazel2gn:path_overwrite: instead of using a single documented # @bazel2gn:skip
filegroup placeholder.
