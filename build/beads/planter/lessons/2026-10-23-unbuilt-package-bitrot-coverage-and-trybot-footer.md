# Unbuilt package bitrot coverage and trybot footer parity

- **Learned from:** cq failure on CL Idf93c5351e44d4bdcf56467d632edd9268390ab5, patchset 4 (task `src_sys_time_time_test_client`)
- **Date:** 2026-10-23
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`

## Root cause

When migrating a package disconnected from the default GN build graph that
requires minimal source edits to fix pre-existing compile bitrot and has no
affected GN tests, the experimental FYI builder fuchsia-coverage fails
Step('covargs') with no input files specified; while build_only_scope and
coder.md instruct the agent to keep the checkout untouched and report
machinery_conflict, commit_message_format's check_cq_include_trybots flagged
existing Cq-Include-Trybots footers on source-bitrot repair commits as an error,
forcing a commit-message-only edit that conflicts with keeping the approved
commit untouched.

## Why not a one-off fix

Exempting Cq-Include-Trybots footers in commit_message_format whenever source
files are modified for pre-existing bitrot repair prevents false-positive footer
errors and commit-message-only remediation conflicts across all unbuilt-package
migrations that require source bitrot fixes.
