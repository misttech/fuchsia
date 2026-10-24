# Gn test package with ninja generated inputs or codegen deps

- **Learned from:** inner loop friction on a local task (task `src_lib_dynfidl_rust`)
- **Date:** 2026-10-16
- **Changed:** `checks/migration_sanity.sh`

## Root cause

In checks/migration_sanity.sh, test_language() unconditionally classified any
pre-sentinel rustc_* target in BUILD.gn as a Bazel-buildable Rust test without
checking whether its source_root, sources, or local dependencies rely on GN
variables, Ninja-generated paths ($target_gen_dir), or unmigrated GN-only
codegen templates (such as gidl or compiled_action), causing
gn_test_package_not_migrated to fire a false positive on
fuchsia_unittest_package/fuchsia_test_package targets that package
Ninja-generated tests.

## Why not a one-off fix

Exempting a single package directory or specific target name would fail to
prevent false-positive gn_test_package_not_migrated errors across other Fuchsia
packages whose Rust or C++ test binaries consume Ninja-generated source files or
depend on GN-only code-generation templates.
