# Checkout relative report and commit message paths

- **Learned from:** inner loop friction on CL I156046cb33887480f29f9874d10fee37f57d49ef, patchset 1 (task `sockaddr`)
- **Date:** 2026-10-16
- **Changed:** `checks/manifest.json`, `prompts/coder.md`

## Root cause

When running inside a jiri worktree (.jiri_root/worktrees/<name>), the coding
agent in round 0 had no explicit instruction in prompts/coder.md requiring
CoderReport.modified_files, summary, and commit message paths to be relative to
$PLANTER_WORKDIR rather than prefixed with .jiri_root/worktrees/<name>/ or host
filesystem paths. Because commit_message_format only runs after a commit exists
(PLANTER_CHANGE_BASE != HEAD), non-checkout-relative paths in
CoderReport.modified_files caused the initial patchset commit message to include
a worktree-prefixed 'Also migrated in this change:' bullet, which then triggered
human review comments on /COMMIT_MSG alone and inner-loop friction during
commit-message-only remediation.

## Why not a one-off fix

Amending the commit message in a single package's CL leaves future
worktree-based migrations vulnerable to reporting .jiri_root/worktrees/<name>/
paths in CoderReport.modified_files during round 0 before a commit exists.
Requiring checkout-relative paths in prompts/coder.md (while keeping coder.md
within its byte budget by removing check-duplicated load() prose) and
documenting commit_msg_non_checkout_path and commit_body_redundant_also_migrated
in checks/manifest.json prevents worktree-prefixed paths across all future
migrations.
