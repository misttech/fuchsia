# Commit message body paragraphs must be wrapped at 72 characters

- **Learned from:** 189 reviewer comments across 94 CLs flagged by shac commit_msg presubmit
  (David Dorwin on CL 1837931, CL 1839405)
- **Date:** 2026-10-26
- **Changed:** `checks/commit_message_format.sh`, `prompts/coder.md`

## Root cause

`commit_message_format.sh` verified footer formats and checked length only on
`Skip-Readme-Check:`, leaving body paragraphs unvalidated. Planter generated unwrapped
sentences exceeding 72 characters, causing `shac` presubmit warnings (`commit_msg:
Line N exceeds 72 characters`) and blocking reviewers who asked for manual rewrites before
approval.

## Remediation

`commit_message_format.sh` now enforces that all non-URL, non-footer, non-code body lines
are <= 72 characters, matching `//scripts/shac/commit_msg_checker.py`. Planter coder
prompts and pre-upload checks validate body line lengths with error severity, preventing
CL uploads that violate the Fuchsia commit message style guidelines.
