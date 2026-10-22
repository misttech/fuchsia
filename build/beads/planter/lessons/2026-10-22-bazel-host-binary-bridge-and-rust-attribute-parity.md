# Bazel host binary and test suite bridge dep parity

- **Learned from:** inner loop friction on a local task (task `tools-fidl-ir-and-tools`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_dep_parity.sh`

## Root cause

gn_dep_parity compared original GN rustc_binary, go_binary, and test package
dependencies against replacement bazel_host_tool and bazel_test_suite bridge
actions in BUILD.gn (whose library dependency graph moves to BUILD.bazel).

## Why not a one-off fix

Hardcoding exemptions for individual host binary or test suite packages would
leave every future migration that replaces a GN binary or test package with
bazel_host_tool or bazel_test_suite blocked on false-positive dependency parity
findings.
