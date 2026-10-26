# Unrelated upstream cq breakage handling

- **Learned from:** cq failure on CL I8a0b2e0c4336ba308539af2954887d5fd32e5d19, patchset 1 (task `src_sys_time_testing_dev_time_source`)
- **Date:** 2026-10-22
- **Changed:** `prompts/coder.md`

## Root cause

The docsgen-sdk CQ failure in kernel_x64.static-checks clippy targets is an
unrelated upstream tree breakage outside the migrated package's dependency
graph, and prompts/coder.md lacked explicit instructions to rebase onto
origin/main or report unrelated upstream CQ failures in disputed_findings
without modifying unrelated targets.

## Why not a one-off fix

Handling this only for a single package would leave future migration tasks
vulnerable to misdiagnosing unrelated upstream static-checks CQ breakages as
local migration errors or making out-of-scope edits across the tree.
