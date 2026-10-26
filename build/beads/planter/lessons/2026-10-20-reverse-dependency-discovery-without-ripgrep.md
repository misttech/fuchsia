# Reverse dependency discovery without ripgrep

- **Learned from:** inner loop friction on a local task (task `pkg-ramdevice-client`)
- **Date:** 2026-10-20
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

When ripgrep (rg) is not on PATH, visibility_audit.sh and find_rdeps.sh fall
back to git grep, which only searches tracked files in each git repository and
misses newly created untracked BUILD.bazel/.bzl/.gni files in directories
without modified tracked files, and visibility_audit.sh restricted git ls-files
discovery of untracked BUILD.bazel files to PLANTER_TARGET_DIR.

## Why not a one-off fix

Any multi-directory migration or transitive dependency migration in an
environment without ripgrep can create untracked BUILD.bazel files across
multiple packages; discovering untracked build files via git ls-files --others
--exclude-standard across all repositories in both visibility_audit.sh and
find_rdeps.sh prevents false-positive rdep errors tree-wide.
