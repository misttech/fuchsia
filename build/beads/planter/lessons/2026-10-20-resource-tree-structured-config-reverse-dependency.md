# Resource Tree & Structured Config Reverse Dependency Visibility Coverage and Multi-Package Bazel-Migration-Target Footer Detection

- **Learned from:** cq failure on CL Ibc0d845f2dfbfc59f6eb84a86d62a14485a6b368, patchset 1 (task `pkg-tests-pkg-resolver-resources`)
- **Date:** 2026-10-20
- **Changed:** `checks/commit_message_format.sh`, `checks/visibility_audit.sh`, `tools/find_rdeps.sh`

## Root cause

CONVERTIBLE_GN_TEMPLATES in checks/visibility_audit.sh and tools/find_rdeps.sh
omitted resource_tree, resource_group, fuchsia_structured_config_*, config,
rustc_library_with_features, bind_library, and cml_shard, causing GN reverse
dependencies of migrated fuchsia_package_resource_group targets (from GN
resource_tree) to be filtered out as gn_nonconvertible_targets so missing
visibility went undetected. Additionally, checks/commit_message_format.sh only
scanned PLANTER_TARGET_DIR when computing Bazel-Migration-Target footer
candidates and unconditionally excluded fx_component_manifest and
fx_test_component even when they migrated same-named BUILD.gn targets.

## Why not a one-off fix

Fixing visibility or commit footers in a single package directory leaves
find_rdeps.sh, visibility_audit.sh, and commit_message_format.sh blind to
reverse dependencies of resource_tree/structured-config/config targets and
co-migrated BUILD.bazel packages across all future multi-package migrations.
