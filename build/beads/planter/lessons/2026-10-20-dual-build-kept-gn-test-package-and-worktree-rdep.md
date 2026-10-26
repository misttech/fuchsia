# Dual build kept gn test package and worktree rdep discovery

- **Learned from:** inner loop friction on a local task (task `pkg-omaha-client-fuchsia-test`)
- **Date:** 2026-10-20
- **Changed:** `checks/migration_sanity.sh`, `checks/visibility_audit.sh`

## Root cause

First, in checks/migration_sanity.sh, duplicate_test_above_sentinel flagged a GN
rustc_test kept above the BAZEL2GN SENTINEL even when it was consumed by a GN
test package/component that must remain in BUILD.gn (due to GN-only component
dependencies) and the corresponding Bazel rustc_test carried # @bazel2gn:skip
(required when using crate = ':<lib>', which GN rustc_test does not support).
Second, in checks/visibility_audit.sh, discover_referencing_build_files invoked
ripgrep with '-g !.jiri_root/**' and without '--no-ignore-parent', causing
ripgrep to ignore the entire checkout when PLANTER_WORKDIR is inside a
'.jiri_root/worktrees/<name>' directory and set used_rg = True on exit code 1
without falling back to git grep.

## Why not a one-off fix

Any dual-build Rust library migration where a device test package must remain in
BUILD.gn (because of unmigrated component dependencies or unsupported
test_specs) while the unit test target is also authored in BUILD.bazel with
crate = ':<lib>', or any check execution inside a .jiri_root/worktrees checkout,
will hit the same false-positive duplicate_test_above_sentinel and
visibility_audit rdep errors unless both check scripts handle these cases
generally.
