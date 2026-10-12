# Bazel host tool target-type parity and transitive GN group reachability for bazel test suite

- **Learned from:** inner loop friction on CL Ia274adbc2a44ff4671597abee80321f0a8dbb85e, patchset 2 (task `clidoc`)
- **Date:** 2026-10-10
- **Changed:** `checks/cq_reachability.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

gn_target_type_parity.sh only exempted GN test templates converting to
bazel_test_suite and did not recognize GN host binary or install_host_tools
targets converting to bazel_host_tool (and forwarding groups in the same
BUILD.gn), while suite_is_wired() in cq_reachability.sh checked only direct
references inside group("tests") without following intermediate GN group()
targets transitively.

## Why not a one-off fix

Hardcoding a single host tool package or parent BUILD.gn would leave every other
host tool migration using bazel_host_tool(install_host_tool = true) and any
bazel_test_suite wired through intermediate GN groups (such as tests_no_e2e)
blocked by false-positive check errors.
