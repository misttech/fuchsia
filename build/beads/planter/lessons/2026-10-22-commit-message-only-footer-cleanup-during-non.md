# Commit-Message-Only Footer Cleanup During Non-Blocking Coverage Trybot Remediation

- **Learned from:** inner loop friction on CL Idf93c5351e44d4bdcf56467d632edd9268390ab5, patchset 3 (task `src_sys_time_time_test_client`)
- **Date:** 2026-10-22
- **Changed:** `prompts/coder.md`

## Root cause

When remediating a non-blocking experimental FYI fuchsia-coverage covargs
('llvm-profdata merge: error: no input files specified') failure on a migration
with a pre-existing bitrot Rust source fix and no affected GN tests, the coder
also amended /COMMIT_MSG to remove an unwarranted Cq-Include-Trybots footer
flagged by commit_message_format while keeping checkout files untouched to
preserve Code-Review+2. Prompts/coder.md did not explicitly note that
footer-cleanup-only /COMMIT_MSG amendments during such remediation rounds are
covered under the /COMMIT_MSG-only remediation_validation machinery_conflict
reporting guidance.

## Why not a one-off fix

Handling this only for a single migrated package would leave any future
migration that combines a /COMMIT_MSG footer cleanup with a non-blocking
coverage trybot remediation vulnerable to repeated remediation_validation
inner-loop friction across the repository.
