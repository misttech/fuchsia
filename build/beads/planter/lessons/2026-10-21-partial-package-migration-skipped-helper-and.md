# Partial package migration skipped helper and footer scope

- **Learned from:** human review on CL I7aea904d7b1480880c8812bc19eeafa12b95d500, patchset 4 (task `pkg-cache-config`)
- **Date:** 2026-10-21
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When a partial dual-build migration only converts one target (such as a
fuchsia_structured_config_rust_lib) while keeping its supporting manifest and
other package/resource targets hand-written above the BAZEL2GN SENTINEL in
BUILD.gn, commit_message_format.sh treated # @bazel2gn:skip targets in
BUILD.bazel that still had hand-written GN definitions above the sentinel as
migrated targets for Bazel-Migration-Target: footers and commit body claims, and
migration_sanity.sh did not flag unreferenced # @bazel2gn:skip helper targets
(such as resource) in BUILD.bazel that duplicate a target kept above the
sentinel in BUILD.gn without any Bazel consumer.

## Why not a one-off fix

Fixing only a single package directory would leave future partial dual-build
migrations able to leave unused # @bazel2gn:skip resource/manifest duplicates in
BUILD.bazel and list unmigrated or copied-only targets in
Bazel-Migration-Target: footers and commit body migration claims across any
partially migrated package.
