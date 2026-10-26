# Diffengine secondary attribute mispairing displaced gn target

- **Learned from:** inner loop friction on CL I3c18384211358d98607eb6bdc50eb4dd1b8df163, patchset 2 (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When an unmigrated GN target (such as a fuchsia_package kept above the BAZEL2GN
SENTINEL because its binary links a rustc_dylib) has no same-named target in
BUILD.bazel, diffengine greedily pairs that pre-sentinel GN target with a
differently named Bazel target (such as rustc_binary(name = "bin", crate_name =
"<pkg_name>")) via normalized dash-to-underscore secondary attribute matching
before reaching the bazel2gn-generated GN target of the same name below the
sentinel. This consumes the Bazel target and leaves the displaced GN target
unmatched at its base and current BUILD.gn line numbers. In migration_sanity.sh,
record_mispaired_crate_name_target_compat_info did not normalize hyphens to
underscores, did not record INFO unmatched_target coverage for the displaced GN
target across base and current BUILD.gn line numbers, and was not invoked in the
BUILD.bazel AST loop.

## Why not a one-off fix

Any dual-build package that keeps a pre-sentinel GN package or wrapper target
named '<foo-bar>' while defining a Bazel library or binary target (e.g. 'lib' or
'bin') with crate_name or output_name '<foo_bar>' triggers the same greedy
secondary-attribute pairing in diffengine and leaves the bazel2gn-generated GN
target unmatched; handling normalized secondary-attribute collisions and
displaced target line numbers across base and current BUILD.gn files in
migration_sanity.sh and run_checks.sh resolves the entire class of false
positives.
