# Gn forwarding group rdep attribution and untracked build file discovery

- **Learned from:** inner loop friction on a local task (task `pkg-stash`)
- **Date:** 2026-10-19
- **Changed:** `checks/visibility_audit.sh`, `prompts/coder.md`, `tools/find_rdeps.sh`

## Root cause

First, when ripgrep (rg) is unavailable and find_rdeps.sh and
visibility_audit.sh fall back to git grep, git grep only searches tracked files
and misses untracked BUILD.bazel files in sibling packages being migrated
concurrently before git add. Second, when a dual-build package keeps
non-testonly GN forwarding group() targets above the BAZEL2GN SENTINEL (for
example a default-named group forwarding a renamed library target ':lib'
alongside expect_includes checks while ':lib' sets GN visibility = [':*']), both
find_rdeps.sh and visibility_audit.sh classified the forwarding group as a
GN-only target and discarded all external callers referencing the package
through that group, causing find_rdeps.sh to recommend empty/private visibility
and visibility_audit.sh to reject widened Bazel visibility.

## Why not a one-off fix

Fixing visibility only on a single library or binary package would leave
find_rdeps.sh and visibility_audit.sh blind to untracked sibling BUILD.bazel
files during multi-package migrations and to external callers of any dual-build
library exposed in GN via a non-testonly forwarding group() above the sentinel.
