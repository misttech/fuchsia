# Redundant forwarding group path overwrite

- **Learned from:** human review on CL I1a707a97f4216e05625fded4d8028bd0fa9e7ab8, patchset 1 (task `src-diagnostics-lib-batch`)
- **Date:** 2026-10-27
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_attr_parity.sh`, `checks/gn_dep_parity.sh`, `tools/run_checks.sh`

## Root cause

The path_overwrite hygiene check in bazel_minimality.sh treated any same-package
# @bazel2gn:path_overwrite://<pkg> annotation on an explicit //<pkg>:<target>
dependency as legitimate without checking whether the overwritten GN target in
<pkg>/BUILD.gn is a pure forwarding group that expands directly to :<target>. In
addition, gn_attr_parity.sh, run_checks.sh, and gn_dep_parity.sh did not
recognize same-package GN forwarding group expansion as an expected dependency
alignment in diffengine or suppress false dep_label_removed notices when the
overwrite annotation is omitted.

## Why not a one-off fix

Removing the redundant # @bazel2gn:path_overwrite annotation in a single package
leaves bazel_minimality.sh permissive toward unnecessary path_overwrite
annotations on pure GN forwarding groups across all future migrations and leaves
diffengine dep_alignment warnings unsuppressed when a GN forwarding group is
expanded to its underlying library target.
