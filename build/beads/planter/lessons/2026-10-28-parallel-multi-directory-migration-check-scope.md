# Parallel multi-directory migration: check scope pollution from untracked agent report files outside the change

- **Learned from:** inner loop friction on a local task (task `b2-cam1`)
- **Date:** 2026-10-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_only_scope.sh`, `checks/manifest.json`

## Root cause

build_only_scope and bazel_minimality add every untracked file (git ls-files
--others) to the change set. A CoderReport JSON that one parallel coder wrote to
the checkout root was therefore reported as an ERROR against every sibling
coder. The sibling coders are not allowed to touch files outside their
directory, so they could not clear it and kept disputing the same two findings.

## Why not a one-off fix

Deleting the stray file for this one task does not stop the next coder from
writing its report to the checkout root again. Every parallel task would then
block all of its coders on the same out-of-scope findings. The checks themselves
must treat untracked agent report artifacts as outside the change, while still
rejecting a committed copy.
