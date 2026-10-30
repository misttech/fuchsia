# Dual build partial package unmigrated and fuchsia sdk dependency blockers

- **Learned from:** inner loop friction on CL I02888491aed06632ef5a1321864b95a79a22e1c7, patchset 2 (task `src_graphics_display_lib_driver-framework-migration-utils_metadata`)
- **Date:** 2026-10-30
- **Changed:** `checks/migration_sanity.sh`

## Root cause

When inspecting pre-sentinel GN targets and test binaries in dual-build packages
to determine whether their external dependencies are already available for
in-tree --config=fuchsia_platform Bazel builds, migration_sanity.sh
(_bazel_pkg_has_target and bazel_buildable) only checked whether the dependency
package's BUILD.bazel declared a matching target name, without checking whether
that BUILD.bazel was a legacy out-of-tree @fuchsia_sdk// target definition whose
BUILD.gn had no BAZEL2GN SENTINEL or bazel_* bridge (which fails analysis under
--config=fuchsia_platform with a missing API level error).

## Why not a one-off fix

Exempting only a single package or target name would leave migration_sanity.sh
unable to recognize any other pre-sentinel library or test target across the
tree that is blocked on dependencies whose existing BUILD.bazel files still use
legacy @fuchsia_sdk// targets without bazel2gn synchronization.
