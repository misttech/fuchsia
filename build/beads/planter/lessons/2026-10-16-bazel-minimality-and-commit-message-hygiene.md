# Bazel minimality and commit message hygiene

- **Learned from:** human review on CL I23bcc8a7f5bc6f91ed74246eb860b9ca45e8e24b, patchset 1 (task `netstack3-trace`)
- **Date:** 2026-10-16
- **Changed:** `checks/bazel_minimality.sh`, `checks/commit_message_format.sh`

## Root cause

The existing checks did not flag three recurring migration patterns: (1) commit
message bodies that merely repeat the '[bazel_migration] //<dir>' subject line
without adding context, (2) rustc_check_cfg_build_flags(cfgs = ["test"]) targets
carried over from GN in packages that have no unit tests or #[cfg(test)] items,
and (3) identical select({...}) dependency blocks repeated inline across
multiple targets' deps attributes instead of being extracted into a shared
_COMMON_DEPS variable with '# @bazel2gn:transformer=deps'.

## Why not a one-off fix

Fixing only the single migrated package leaves future migrations of
multi-variant Rust crates and other packages vulnerable to emitting redundant
commit body restatements, dead test check-cfg targets in testless packages, and
duplicated inline select() dependency blocks across variant targets.
