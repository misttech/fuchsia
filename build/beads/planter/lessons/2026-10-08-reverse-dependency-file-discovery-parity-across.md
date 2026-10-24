# Reverse dependency file discovery parity across checkout repositories

- **Learned from:** inner loop friction on a local task (task `src_lib_android_binder_rust`)
- **Date:** 2026-10-08
- **Changed:** `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

checks/visibility_audit.sh relied solely on invoking rg on the root workspace
without a git grep fallback when rg is unavailable on PATH and without passing
gitignored sub-repository roots to rg, causing matched_files to be empty (or
miss sub-repository callers) and falsely flagging every caller visibility entry
as false_positive_rdep_visibility.

## Why not a one-off fix

Hardcoding exemptions for a single target package would leave
visibility_audit.sh broken for all other migrated targets whenever rg is absent
from PATH or when callers reside in gitignored sub-repositories.
