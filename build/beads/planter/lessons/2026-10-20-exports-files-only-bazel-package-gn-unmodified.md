# Exports files only bazel package gn unmodified compat

- **Learned from:** inner loop friction on a local task (task `pkg-tests-pkgctl`)
- **Date:** 2026-10-20
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

record_review_diff_compat_info in checks/migration_sanity.sh and
downgrade_check_cfg_diff_findings in tools/run_checks.sh only downgraded
review_diff gn_unmodified findings when BUILD.gn already contained ## BAZEL2GN
SENTINEL, missing packages whose BUILD.bazel only defines exports_files() or
non-bazel2gn targets where BUILD.gn intentionally has no sentinel and is not
modified.

## Why not a one-off fix

Any component or test migration that adds or extends exports_files() visibility
in a shard-hosting package (such as sdk/lib/inspect, sdk/lib/syslog, or
src/sys/test_runners/*) touches a BUILD.bazel whose sibling BUILD.gn has no
bazel2gn sentinel; handling all packages without unskipped convertible Bazel
targets prevents false-positive gn_unmodified errors across all future
migrations.
