# Redundant single target forwarding indirection

- **Learned from:** human review on CL I5c7a3e7fc1edb9ce62852fd164638a1130bb1281, patchset 2 (task `src_lib_testing_test_list`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

When migrating a GN package that defines an internal library target named `lib`
with `crate_name = "<outer>"` and a single-target forwarding `group("<outer>") {
public_deps = [ ":lib" ] }`, preserving the `:lib` + `alias(name = "<outer>",
actual = ":lib")` indirection (or leaving `name = "lib"` unfolded while keeping
`group("<outer>")` in `BUILD.gn`) adds unnecessary boilerplate when no external
callers depend on `//<pkg_dir>:lib`. Furthermore, checking external callers by
matching `<pkg_base>:<inner>` across the repository can match unrelated packages
sharing the same directory basename and silently skip the check.

## Why not a one-off fix

Fixing only a single migrated package leaves future migrations of GN packages
that use the `rustc_library("lib")` plus forwarding `group("<pkg>")` pattern
vulnerable to preserving redundant `:lib` and `alias` targets or suffering false
negatives when another directory shares the same basename.
