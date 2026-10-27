# Rustc test target named after output binary instead of raw attribute output name

- **Learned from:** human review on CL I55a3e9fa7750e063cd8690f31427eeb603bb10fc, patchset 2 (task `src-sys-run-test-suite-directory-and-fixtures`)
- **Date:** 2026-10-27
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When decomposing a GN `rustc_library` whose `name` (`crate_name`) differs from
its target name and has `with_unit_tests = true` into an explicit
`rustc_test(name = "<target>_test", crate = ":<target>")` in `BUILD.bazel`,
`bazel2gn` translates the `rustc_test` into a standalone GN
`rustc_test("<target>_test")` whose default binary name is `bin/<target>_test`
rather than `bin/<crate_name>_lib_test`. The coder added `#
@bazel2gn:raw_attribute:output_name = "<crate_name>_lib_test"` above
`rustc_test` (and `migration_sanity.sh` exempted `raw_attribute:output_name` on
`rustc_test`), whereas `bazel_minimality.sh` did not reject `#
@bazel2gn:raw_attribute:output_name` on `rustc_test` / `rust_test` /
`host_rustc_test` targets when naming the test target itself after the binary
(`name = "<output_name>"`) gives GN `bin/<output_name>` by default without any
`@bazel2gn:raw_attribute` directive while `wrap_host_rust_test` preserves its
`tests.json` target name.

## Why not a one-off fix

Fixing only a single package leaves future GN-to-Bazel migrations of Rust crates
with custom crate names or custom `.cml` unit-test component manifests
vulnerable to adding `# @bazel2gn:raw_attribute:output_name = "..."` above
`rustc_test` instead of naming the `rustc_test` target after its output binary
and updating the `wrap_host_rust_test(binary_name = ...)`,
`fx_packaged_binary(binary = ...)`, and hand-written `BUILD.gn` test references.
