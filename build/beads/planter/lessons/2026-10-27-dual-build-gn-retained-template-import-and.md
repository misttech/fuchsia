# Dual build gn retained template import and variable backed tests group detection

- **Learned from:** inner loop friction on CL I8ab6c05819d63ef51697cfe8e8efaca1883a3b25, patchset 1 (task `src-devices-tools-fidlgen-banjo`)
- **Date:** 2026-10-27
- **Changed:** `checks/cq_reachability.sh`, `checks/migration_sanity.sh`

## Root cause

First, `migration_sanity.sh` mapped `//build/sdk/sdk_host_tool.gni` in
`sentinel_covered_gni_tmpls` only to the three combined binary+SDK wrapper
templates (`sdk_executable_host_tool`, `sdk_go_binary_host_tool`,
`sdk_rustc_binary_host_tool`) and omitted the standalone `sdk_host_tool`
template defined by the same `.gni` file, falsely flagging
`import("//build/sdk/sdk_host_tool.gni")` as unused whenever a dual-build
`BUILD.gn` retains `sdk_host_tool(...)` above `## BAZEL2GN SENTINEL` (before
`bazel_migration.gni` is imported below the sentinel). Second,
`check_obsolete_or_upward_gn_tests_group` in `cq_reachability.sh` extracted
dependencies of `group("tests")` solely via literal string regexes inside
bracketed `deps = [...]` lists (`all_deps`), ignoring variable-backed or
forwarded dependency assignments (such as `deps = all_test_deps` populated by a
`foreach` loop over GN-only `compiled_action` targets) and falsely reporting
`group("tests")` as empty (`obsolete_gn_tests_group`).

## Why not a one-off fix

Any dual-build host tool package that retains a standalone `sdk_host_tool()`
target above the sentinel requires `import("//build/sdk/sdk_host_tool.gni")` at
the top of `BUILD.gn` due to GN's top-down evaluation order, and any `BUILD.gn`
whose `group("tests")` aggregates GN-only build-time or `foreach`-generated test
targets via a GN list variable (`deps = <var>`) will trigger the same
false-positive `obsolete_gn_tests_group` error whenever an ancestor
`BUILD.bazel` defines a `test_suite`. Updating the template table in
`migration_sanity.sh` and recognizing non-literal/variable `deps` assignments in
`cq_reachability.sh` eliminates both classes of false positives across the
entire tree.
