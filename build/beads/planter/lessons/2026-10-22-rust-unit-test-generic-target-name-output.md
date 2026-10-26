# Rust unit test generic target name output collision

- **Learned from:** cq failure on CL I6af1a2b1db9ad9a9d7212ab2122641f385e9fd80, patchset 4 (task `src_developer_ffx_lib_uart_driver_api`)
- **Date:** 2026-10-22
- **Changed:** `prompts/coder.md`

## Root cause

When a GN rustc_library("lib") with crate_name/name = "<crate>" and
with_unit_tests = true is migrated to a standalone Bazel rustc_test or
host_rustc_test named "lib_test" with crate = ":lib" and no explicit crate_name,
bazel2gn generates a GN rustc_test("lib_test") without output_name, defaulting
its binary path to host_x64/lib_test (whereas GN with_unit_tests = true set name
= "<crate>_lib_test") and colliding with any other package's lib_test in
BUILDCONFIG.gn.

## Why not a one-off fix

Many Fuchsia packages (especially ffx libraries and plugins) name their library
target "lib" and its unit test target "lib_test"; fixing only a single package
leaves all other dual-build packages migrating to standalone
rustc_test/host_rustc_test("lib_test") vulnerable to the same GN duplicate
output file collision.
