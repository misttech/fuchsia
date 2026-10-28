# Check script runtime integrity and byte identical manifest relocation

- **Learned from:** inner loop friction on a local task (task `virt-devices-b`)
- **Date:** 2026-10-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/build_only_scope.sh`, `checks/cq_reachability.sh`, `checks/manifest.json`, `checks/visibility_audit.sh`

## Root cause

Three check-machinery defects blocked migration verification: (1)
visibility_audit.sh crashed with TypeError in compute_package_rdeps() due to a
mismatched 7-argument call site and undefined below_sentinel/below_synced
variables from an incomplete prior edit; (2) cq_reachability.sh crashed with
NameError calling an undefined find_ancestor_bazel_test_suite() helper and
unpacking a 3-tuple where nearest_ancestor_test_suite() returns a 2-tuple; and
(3) build_only_scope.sh and bazel_minimality.sh rejected byte-identical .cml
component manifests relocated from an unmigrated ancestor or sibling directory
referenced by the pre-migration BUILD.gn into their owning Bazel package.

## Why not a one-off fix

Every GN-to-Bazel migration runs visibility_audit and cq_reachability, so their
Python runtime crashes break verification across all packages in the repository,
and any subpackage component whose pre-migration GN manifest lived in an
unmigrated parent or sibling meta/ directory must relocate that .cml
byte-identically so its Bazel manifest target owns the file within package
boundaries.
