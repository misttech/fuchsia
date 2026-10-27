# Bazel builtin pseudo package and visibility label filtering

- **Learned from:** inner loop friction on CL Iaa2e4e4a06b03bca4b85c475637090a69a5c8251, patchset 5 (task `src-connectivity-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

check_unlanded_bazel_dependencies in checks/cq_reachability.sh walked string
constants starting with '//' in BUILD.bazel without excluding built-in Bazel
pseudo-packages such as '//conditions:default' (used in select() expressions),
'//visibility:*', or '//command_line_option:*', causing it to look up a
non-existent 'conditions/BUILD.bazel' directory on disk and emit false-positive
missing_bazel_dependency_package errors.

## Why not a one-off fix

Any BUILD.bazel target across the codebase that uses select() with
'//conditions:default' or references built-in Bazel pseudo-packages and flag
attributes will trigger the same false-positive missing_bazel_dependency_package
error unless check_unlanded_bazel_dependencies systematically filters out
non-package Bazel pseudo-labels and non-dependency attributes.
