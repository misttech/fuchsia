# Bazel fx package rustc dylib debug symbols base tree and commit scope check

- **Learned from:** cq failure on CL I3c18384211358d98607eb6bdc50eb4dd1b8df163, patchset 2 (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-23
- **Changed:** `checks/bazel_minimality.sh`, `checks/migration_sanity.sh`

## Root cause

When the existing commit had already modified
build/bazel/debug_symbols/aspects.bzl in HEAD with a clean working tree,
migration_sanity.sh inspected the working copy of aspects.bzl instead of the
base revision (suppressing fx_package_missing_rust_dylib_debug_symbols), and
bazel_minimality.sh defaulted PLANTER_CHANGE_BASE to HEAD without falling back
to HEAD~1 on a committed [bazel_migration] change (missing
out_of_scope_package_modified on shared build/bazel/*.bzl edits).

## Why not a one-off fix

Reverting only the single package's fx_package and aspects.bzl edit by hand
would not prevent migration_sanity.sh and bazel_minimality.sh from missing
committed shared-rule edits or misdetecting upstream rust_dylib_library debug
symbol support whenever a task starts from or commits a change on HEAD.
