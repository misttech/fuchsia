# Commit message only review remediation

- **Learned from:** inner loop friction on CL Ida4b14726d8c1e74b00fc357840c25c3ec10ac03, patchset 1 (task `netstack3-persistance-fidl`)
- **Date:** 2026-10-16
- **Changed:** `prompts/coder.md`

## Root cause

When human review comments are anchored solely on /COMMIT_MSG without explicit
commit-message keywords in the comment body, harness remediation validation
rejects a commit-message-only amendment unless the coder reports a disputed
finding for /COMMIT_MSG.

## Why not a one-off fix

Resolving this only for a single package CL would leave future
commit-message-only review rounds vulnerable to repeated validation retries
across all migrated packages unless prompts/coder.md explicitly documents how to
report /COMMIT_MSG remediation validation conflicts.
