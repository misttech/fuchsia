# Unmerged parent bazel package dependency folding

- **Learned from:** inner loop friction on CL Iba38f5ca324168715993fd4672573507be2847aa, patchset 2 (task `src_sys_time_lib_push_source`)
- **Date:** 2026-10-23
- **Changed:** `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

When a migration task depends on a newly migrated Bazel package that exists only
in an unmerged local ancestor commit (cherry-picked into HEAD~1) rather than in
origin/main, Gerrit CQ tests the uploaded top commit against origin/main and
fails with 'no such package <dep>: BUILD file not found'. The coder prompt
lacked explicit instructions to fold unmerged local ancestor dependency commits
into HEAD with 'git reset --soft "$(git merge-base origin/main HEAD)" && git
commit --amend' instead of disputing the CQ failure as a commit-message-only
machinery_conflict, and run_checks.sh treated a soft-reset parent change as
commit-message-only since HEAD^{tree} matched PLANTER_UPLOADED_COMMIT^{tree}.

## Why not a one-off fix

Fixing only a single target package leaves every stacked GN-to-Bazel migration
task whose dependency CL has not yet merged into origin/main vulnerable to
misdiagnosing Gerrit CQ missing-package errors as unresolvable machinery
conflicts or skipping non-commit-message checks after folding the parent commit
into HEAD.
