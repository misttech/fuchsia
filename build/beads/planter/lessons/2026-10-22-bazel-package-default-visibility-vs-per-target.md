# Bazel package default visibility vs per target scoping

- **Learned from:** human review on CL I3dc7bf8e64a56e7e8513e7e98cc62cea7ce10ccf, patchset 3 (task `src_developer_lib_usb_vsock`)
- **Date:** 2026-10-22
- **Changed:** `checks/visibility_audit.sh`

## Root cause

When a migrated BUILD.bazel set default_visibility in package(...),
visibility_audit.sh set has_pkg_default_vis = True and consequently suppressed
per-target missing_target_visibility findings on individual targets with
external callers, while only emitting a package-wide aggregate visibility
recommendation on package() instead of per-target visibility guidance for
targets used outside the package versus package-private internal/test targets.
In addition, get_preexisting_vis_info compared non-primary BUILD.bazel files
against HEAD when PLANTER_CHANGE_BASE defaulted to HEAD on an already-committed
change, treating newly committed default_visibility in secondary or multi-target
packages as pre-existing.

## Why not a one-off fix

Fixing only a single package's BUILD.bazel leaves visibility_audit.sh
suppressing per-target missing_target_visibility findings whenever
package(default_visibility = ...) is present and leaves multi-directory or
committed-HEAD runs vulnerable to misclassifying newly added default_visibility
as pre-existing across future GN-to-Bazel migrations.
