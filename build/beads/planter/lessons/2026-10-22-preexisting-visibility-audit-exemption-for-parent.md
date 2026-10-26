# Preexisting visibility audit exemption for parent build files

- **Learned from:** inner loop friction on CL I6af1a2b1db9ad9a9d7212ab2122641f385e9fd80, patchset 3 (task `src_developer_ffx_lib_uart_driver_api`)
- **Date:** 2026-10-22
- **Changed:** `checks/visibility_audit.sh`

## Root cause

When a migration touches a pre-existing ancestor or sibling BUILD.bazel outside
the target directory (such as registering a migrated test target in a parent
test_suite), get_preexisting_vis_info in visibility_audit.sh only exempted
pre-existing target-level visibility entries and targets omitting visibility,
but still flagged pre-existing package(default_visibility = ...) and
pre-existing exports_files(...) calls without visibility.

## Why not a one-off fix

Exempting only a single parent package path would fail whenever any other
migration registers tests or updates visibility in another pre-existing
BUILD.bazel that still contains legacy package-level default_visibility or
unscoped exports_files() calls.
