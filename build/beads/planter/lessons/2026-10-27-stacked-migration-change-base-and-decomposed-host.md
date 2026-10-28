# Stacked migration change base and decomposed host test parity

- **Learned from:** inner loop friction on a local task (task `src-devices-tools-fidlgen-banjo`)
- **Date:** 2026-10-27
- **Changed:** `checks/build_only_scope.sh`, `checks/commit_message_format.sh`, `checks/cq_reachability.sh`, `checks/tests_json_parity.sh`, `tools/run_checks.sh`

## Root cause

Two systemic machinery defects caused the false-positive findings: (1)
tools/run_checks.sh, checks/commit_message_format.sh, and
checks/build_only_scope.sh shifted PLANTER_CHANGE_BASE from HEAD to HEAD~1
whenever HEAD's subject started with '[bazel_migration]', without verifying that
HEAD actually touched or targeted the current task's target directories rather
than a stacked parent migration commit; and (2) checks/tests_json_parity.sh
matched host tests only by exact test name or exact target label, missing GN
rustc_library/rustc_binary unit tests (e.g. target :lib_test with binary
<crate>_lib_test) decomposed into Bazel host_rustc_test/rustc_test targets
renamed to <crate>_test per bazel_minimality's generic_rustc_test_name_collision
rule.

## Why not a one-off fix

Hardcoding a single package or commit would fail on every stacked wave migration
where an uncommitted child task builds on top of a committed '[bazel_migration]'
parent CL, and on every Rust library/binary package whose generic
':lib_test'/':bin_test' unit test is decomposed into a crate-named Bazel host
test. Verifying that HEAD belongs to the task's target directories before
shifting change_base to HEAD~1 and matching same-package host tests by
normalized crate/binary test stem resolves both classes of defects across all
packages.
