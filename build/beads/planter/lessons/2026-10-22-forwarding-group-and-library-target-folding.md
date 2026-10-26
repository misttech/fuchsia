# Forwarding group and library target folding

- **Learned from:** inner loop friction on CL I5c7a3e7fc1edb9ce62852fd164638a1130bb1281, patchset 2 (task `src_lib_testing_test_list`)
- **Date:** 2026-10-22
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_dep_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`

## Root cause

gn_target_type_parity.sh and gn_dep_parity.sh matched GN targets strictly by
target name without recognizing when a pure single-target forwarding
group("<outer>") and its same-package convertible target :<inner> (with zero
external callers) are folded directly into <tmpl>("<outer>"), causing false
missing_migrated_target and gn_target_type_changed errors when removing
redundant alias("<outer>") -> :<inner> indirection.

## Why not a one-off fix

Many Fuchsia GN packages define a convertible library target named "lib" (with
crate_name or name set to the package name) alongside a pure forwarding
group("<pkg>") { public_deps = [ ":lib" ] }; handling this folding pattern
generically in gn_target_type_parity.sh, gn_dep_parity.sh, and
bazel_minimality.sh prevents both redundant alias indirection and false-positive
parity errors across all future migrations.
