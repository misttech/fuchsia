# Gn rustc test output name collision when expanding rustc library with unit tests

- **Learned from:** cq failure on CL I02859986d803c81f632aaa0fd276b0ec7a86cc1a, patchset 1 (task `src-lib-mdns-rust`)
- **Date:** 2026-10-23
- **Changed:** `checks/gn_attr_parity.sh`

## Root cause

In GN (//build/rust/rustc_library.gni), setting with_unit_tests = true on
rustc_library("<lib>") creates a GN test target named "<lib>_test" whose output
binary in root_out_dir is named "<crate>_lib_test" (not "<lib>_test"). When
migrating that target to a standalone Bazel rustc_test(name = "<lib>_test",
crate = ":<lib>"), bazel2gn generates a GN rustc_test("<lib>_test") without
output_name, changing the root_out_dir binary name from "<crate>_lib_test" to
"<lib>_test". None of the static checks verified whether this default
output_name collides with another executable or test target in the checkout,
leading to a GN gen duplicate output file failure on CQ builders.

## Why not a one-off fix

Fixing only the single migrated library target does not prevent future
rustc_library(with_unit_tests = true) migrations from silently changing the GN
test binary output_name from <crate>_lib_test to <lib>_test and colliding with
an existing C++ test() or executable() target of the same name elsewhere in the
repository.
