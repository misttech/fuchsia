# Dual build host tool check consolidation and pinned machinery resilience

- **Learned from:** inner loop friction on CL I6cfabf00ecba0f03ecb2720d36e57639b83181c8, patchset 2 (task `src-developer-ffx-plugins-platform`)
- **Date:** 2026-10-28
- **Changed:** `checks/dual_built_host_tool.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When an evolution merges an updated checks/manifest.json into an already-pinned
task workspace without touching a newly created standalone check script file,
the pinned workspace references an enabled script_file that was never copied
into the pin, causing both run_checks.sh and the post-report check runner to
fail with exit status 127 (No such file or directory). Additionally,
migration_sanity did not treat host binaries referenced via
bazel_host_tool(bazel_target = ...) under a different GN target name as packaged
binaries when checking # @bazel2gn:skip.

## Why not a one-off fix

Every in-flight migration task pinned before a standalone check script was added
can receive an updated checks/manifest.json during subsequent evolutions and
fail with exit status 127. Consolidating check_dual_built_host_tools (and
bazel_host_tool packaged-binary recognition) into migration_sanity.sh—which
exists in every task pin—while retiring the standalone manifest entry and
guarding run_checks.sh against missing scripts in partial pins prevents this
failure across all active and future tasks.
