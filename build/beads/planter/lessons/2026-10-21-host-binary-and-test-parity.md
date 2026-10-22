# Host binary template parity

- **Learned from:** inner loop friction on a local task (task `tools-gn-tier0-2`)
- **Date:** 2026-10-21
- **Changed:** `checks/gn_target_type_parity.sh`

## Root cause

gn_target_type_parity's GN_HOST_TOOL_TEMPLATES omitted python_binary and SDK
host tool templates (sdk_executable_host_tool, sdk_go_binary_host_tool,
sdk_rustc_binary_host_tool, sdk_host_tool), causing false-positive target type
parity errors when those GN templates were replaced by bazel_host_tool bridges.

## Why not a one-off fix

Any package migrating a Python or SDK host binary to bazel_host_tool would hit
the same false-positive target type parity error.
