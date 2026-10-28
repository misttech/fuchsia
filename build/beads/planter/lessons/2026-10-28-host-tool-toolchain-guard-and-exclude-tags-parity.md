# Host tool toolchain guard and exclude tags parity

- **Learned from:** inner loop friction on a local task (task `src-diagnostics-triage`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

gn_attr_parity treated a GN host binary migrated to bazel_host_tool as an
unchanged retained target when flagging newly added if (is_host) guards (despite
bazel_host_tool.gni asserting is_host), and flagged exclude_toolchain_tags as
dropped in both gn_only_attr_dropped and review_diff attribute_alignment even
though bazel_host_tool builds exclusively in toolchain_variant.base with
bazel_config = host and Bazel *_binary_host_tool macros do not accept
exclude_toolchain_tags or disable_instrumentation.

## Why not a one-off fix

Every Fuchsia package evaluated in non-host toolchains that migrates a GN host
binary with exclude_toolchain_tags to *_binary_host_tool in BUILD.bazel and
bazel_host_tool in BUILD.gn requires an if (is_host) guard and relies on
bazel_host_tool building in toolchain_variant.base, so gn_attr_parity must
handle this host-tool pattern generally across all packages.
