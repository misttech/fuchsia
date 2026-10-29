# Rust unit test output name collision and upstream rebase

- **Learned from:** cq failure on CL I9e175c97205cec41bb1c3511b6c83a7cced1d3f9, patchset 1 (task `netemul-fidl`)
- **Date:** 2026-10-22
- **Changed:** `prompts/coder.md`

## Root cause

When migrating a GN rustc_library with a generic target name like 'lib' and a
distinct crate_name, replacing with_unit_tests = true with a standalone
host_rustc_test or rustc_test named 'lib_test' without crate_name causes
bazel2gn to emit rustc_test('lib_test') with default output_name 'lib_test',
colliding in gn gen (Duplicate output file: host_x64/lib_test) across packages
and breaking CQ until rebased onto the upstream fix.

## Why not a one-off fix

Many Rust libraries across the tree use a generic target name such as 'lib' with
a distinct crate_name; teaching the coder prompt to preserve unique test output
names (via with_host_unit_tests = True, naming the test '<crate>_test', or
setting crate_name = '<crate>_lib_test') and to rebase when CQ hits a reverted
upstream tree breakage prevents this class of GN duplicate-output failure across
all future migrations.
