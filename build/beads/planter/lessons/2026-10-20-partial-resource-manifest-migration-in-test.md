# Partial resource/manifest migration in test packages with kept GN integration tests and review diff alignment parity

- **Learned from:** inner loop friction on a local task (task `pkg-tests-pkg-resolver-resources`)
- **Date:** 2026-10-20
- **Changed:** `checks/bazel_minimality.sh`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

First, migration_sanity only exempted testonly binaries (not testonly helper
libraries like rustc_library) above the BAZEL2GN SENTINEL when consumed by GN
test targets kept above the sentinel. Second, bazel_minimality seeded its
dependency-package reachability graph only from PLANTER_TARGET_DIRS and not from
other in-scope packages whose BUILD files reference PLANTER_TARGET_DIRS, causing
shard-include and subpackage BUILD.bazel edits required by a co-migrated package
to be flagged as out_of_scope_package_modified. Third, review_diff flagged
exports_files-only edits to packages whose BUILD.gn already contained the
BAZEL2GN SENTINEL as gn_unmodified, flagged
resource(src)/fuchsia_package_resource_group(srcs, strip_prefix,
dest)/fx_component_manifest mappings as missing_sources and attribute_alignment,
and flagged bazel2gn-converted go_binary_host_tool targets and # @bazel2gn:skip
deps in modified subpackages.

## Why not a one-off fix

Hardcoding exemptions for a single package directory would leave any future
migration that exports files from an already-migrated dual-build package,
migrates resource/resource_tree/fuchsia_component_manifest targets alongside
kept GN integration tests, or updates cml shard visibility for a co-migrated
package blocked by the same false-positive static and diffengine findings.
