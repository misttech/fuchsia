# Bazel dependency package existence and stacked ancestry verification

- **Learned from:** cq failure on CL I4b835ab2d696fc7a345394547335937da73039a8, patchset 1 (task `src-developer-ffx-lib-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

When a migrated BUILD.bazel target exported via bazel_test_suite depends on a
main-repository //package:target whose BUILD.bazel is missing from the checkout
or provided only by an unmerged stacked parent commit above origin/main (and
dropped or unlinked during rebase/upload), CQ tests.json generation (bazel
cquery) fails with 'no such package: BUILD file not found'; additionally
has_bazel_pkg_in_worktree previously treated any directory without a BUILD.gn as
a valid Bazel package and did not distinguish stacked parent commit packages
from origin/main packages.

## Why not a one-off fix

Fixing only a single target's dependency or stack in isolation would not prevent
future migrations from referencing main-repository directories that lack a
BUILD.bazel/BUILD file or losing stacked parent dependency commits during rebase
onto origin/main across other packages.
