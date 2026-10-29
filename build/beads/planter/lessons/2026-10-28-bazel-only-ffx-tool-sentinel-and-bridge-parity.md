# Bazel only ffx tool sentinel and bridge parity

- **Learned from:** cq failure on CL I6cfabf00ecba0f03ecb2720d36e57639b83181c8, patchset 4 (task `src-developer-ffx-plugins-platform`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_target_type_parity.sh`, `checks/migration_sanity.sh`

## Root cause

Upstream bazel2gn now maps ffx_tool and idk_ffx_tool to noGNEquivalent and
removed import("//src/developer/ffx/build/ffx_tool.gni") from
//build/tools/bazel2gn/bazel_migration.gni. However, migration_sanity.sh still
treated ffx_tool as a bazel2gn-convertible rule covered by bazel_migration.gni
below the BAZEL2GN SENTINEL and did not flag stale ffx_tool(...) blocks
remaining below the sentinel after rebasing onto the updated bazel2gn rules
(causing gn gen to fail with Unknown function: ffx_tool), while
gn_target_type_parity.sh did not recognize bazel_ffx_tool() as a valid host-tool
bridge replacement for GN ffx_tool() targets.

## Why not a one-off fix

Fixing only a single ffx plugin or subtool package would leave all other ffx
plugin and subtool migrations vulnerable to stale ffx_tool() blocks below the
BAZEL2GN SENTINEL, false-positive unused_gn_import errors on ffx_tool.gni
imports for bazel_ffx_tool(), or gn_target_type_changed errors when bridging
ffx_tool() to GN via bazel_ffx_tool().
