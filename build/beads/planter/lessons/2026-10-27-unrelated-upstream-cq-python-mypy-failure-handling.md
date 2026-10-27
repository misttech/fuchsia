# Unrelated upstream cq python mypy failure handling

- **Learned from:** cq failure on CL I77d7ab703a21e180fc93d4de8c42f5878f0edd5d, patchset 2 (task `src-lib-network-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/build_verification.sh`, `prompts/coder.md`

## Root cause

In build_verification.sh, unrelated_ninja_step_target matched NINJA_STEP_TARGET
on the preceding line (lines[idx - 1]) before matching NINJA_FAILED_PYTHON_STAMP
on the FAILED: line itself (lines[idx]), so in parallel Ninja output a
concurrent step from a migrated package printed immediately before a failing
.mypy_checked stamp could misattribute the unrelated Python mypy failure to the
migrated package. In addition, prompts/coder.md only instructed the coder to
rebase on CQ 'Failed to rebase' rather than also rebasing and avoiding
out-of-scope edits when CQ fails in an unrelated upstream target (such as a
.mypy_checked target under //build/**).

## Why not a one-off fix

Any migration task running local Ninja builds or Gerrit CQ while an upstream
Python build-time test or mypy target on origin/main is broken can encounter
parallel Ninja output ordering or unrelated CQ failures; prioritizing the
FAILED: stamp path in build_verification.sh and guiding the coder on unrelated
upstream CQ failures protects all migration tasks.
