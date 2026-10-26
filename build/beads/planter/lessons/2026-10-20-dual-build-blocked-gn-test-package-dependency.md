# Dual build blocked gn test package dependency handling

- **Learned from:** inner loop friction on a local task (task `pkg-mock-omaha-server-fuchsia-test`)
- **Date:** 2026-10-20
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

migration_sanity only checked external dependency BUILD.gn files for GN-only
constructs when no BUILD.bazel existed in the dependency directory at all,
missing cases where a dependency directory has a partial BUILD.bazel (such as
build_flags or exports_files) while the referenced target in BUILD.gn uses a
GN-only template like network_unified_binary, and only matched relative :target
labels when collecting kept GN test package dependencies.

## Why not a one-off fix

Hardcoding a single test package or directory would fail on any other Fuchsia
package whose test package must remain above the BAZEL2GN SENTINEL because it
bundles a component from a partially migrated dependency directory using a
GN-only template.
