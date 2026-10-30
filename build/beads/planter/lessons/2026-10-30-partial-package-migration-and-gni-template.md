# Partial package migration and gni template visibility auditing

- **Learned from:** human review on CL Ia429928fe0ec59c2ebe63d7763cc167c91b27617, patchset 6 (task `src_lib_dso_cpp`)
- **Date:** 2026-10-30
- **Changed:** `checks/migration_sanity.sh`, `checks/visibility_audit.sh`

## Root cause

First, in checks/visibility_audit.sh, when a non-build/ .gni file defines a GN
template that references a migrated target and also references a pre-sentinel
GN-only target in the same package, compute_package_rdeps replaced the .gni
file's own package (norm_caller) with gni_importers instead of retaining
norm_caller alongside gni_importers in true_rdep_packages and per_target_rdeps,
causing visibility_audit.sh to contradict tools/find_rdeps.sh and emit
false_positive_rdep_visibility when visibility is granted to the .gni-owning
package. Second, in checks/migration_sanity.sh, inspect_gn_target_deps only
recorded an external dependency in unmigrated_deps when its package had no
BUILD.bazel at all (missing packages whose BUILD.bazel only exports files or
defines other targets), and the pre-sentinel convertible-target check only
exempted targets blocked by gn_blocked_dep rather than also exempting unmigrated
targets in a partially migrated package that are blocked by an unmigrated
external cross-package dependency.

## Why not a one-off fix

Suppressing or disputing these findings for a single package leaves
checks/visibility_audit.sh and checks/migration_sanity.sh broken for every
partially migrated dual-build package in the tree where one target is blocked by
an unmigrated external dependency or where a companion .gni template references
both migrated and GN-only targets, forcing future migrations to either drop
required visibility entries, break changelist scoping, or rely on rejected
finding disputes.
