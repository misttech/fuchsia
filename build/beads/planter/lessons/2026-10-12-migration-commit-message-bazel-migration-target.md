# Migration Commit Message Bazel-Migration-Target Footer and Contiguous Footer Block Enforcement

- **Learned from:** human review on CL I6645d3a562cd2ab0c17cacd9850c451ae7b768a3, patchset 5 (task `stressor`)
- **Date:** 2026-10-12
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`, `tools/run_checks.sh`

## Root cause

checks/commit_message_format.sh validated commit subjects, body descriptions,
and Test: footers, but never checked for the required Bazel-Migration-Target:
footer lines or verified that footers are grouped in a single contiguous block
at the bottom of the commit message without blank lines separating them from
Change-Id. In addition, tools/run_checks.sh omitted commit_message_format from
COMMIT_MSG_CHECKS when only the commit message changed since upload.

## Why not a one-off fix

Manually adding Bazel-Migration-Target: footers to a single migration commit
does not prevent future migration CLs from omitting Bazel-Migration-Target:
footers, formatting them with invalid or wrapped labels, or separating them from
the final footer block with blank lines that break Gerrit footer indexing.
