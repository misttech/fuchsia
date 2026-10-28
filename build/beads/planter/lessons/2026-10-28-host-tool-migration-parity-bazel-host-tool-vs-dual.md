# Host tool migration parity (bazel host tool vs dual-built bazel2gn host binary with install host tools)

- **Learned from:** human review on CL I0ca6b534019534464132bf6d840128f01434d787, patchset 2 (task `src-intl-strings-to-fidl`)
- **Date:** 2026-10-28
- **Changed:** `checks/dual_built_host_tool.sh`, `checks/manifest.json`

## Root cause

The check script checks/dual_built_host_tool.sh registered in
checks/manifest.json was missing on disk, and no active static check rejected
keeping a hand-written install_host_tools() above the BAZEL2GN SENTINEL whose
dependency is a host binary (rustc_binary, go_binary, executable) synced below
the sentinel by bazel2gn instead of building the host tool in Bazel only and
exposing it to GN via bazel_host_tool(install_host_tool = true) with #
@bazel2gn:skip on the Bazel binary target.

## Why not a one-off fix

Fixing only a single package directory would allow future host tool migrations
across the tree to keep install_host_tools() above the BAZEL2GN SENTINEL and
dual-compile host binaries in both GN and Bazel via bazel2gn instead of bridging
the Bazel-built binary into GN through bazel_host_tool(install_host_tool =
true).
