# GN target type parity allows unittest component conversion to bazel_test_suite

- **Learned from:** inner loop friction on [CL 1881054](https://fuchsia-review.googlesource.com/c/fuchsia/+/1881054), patchset 2 (task `pkg-cobalt-builders`)
- **Date:** 2026-10-15
- **Changed:** `checks/gn_target_type_parity.sh`

## Root cause

In `checks/gn_target_type_parity.sh`, `GN_TEST_TEMPLATES` only included
`fuchsia_unittest_package`, `fuchsia_test_package`, `fuchsia_test`, `python_host_test`,
`host_test`, `rustc_test`, `go_test`, and `test`. It omitted `fuchsia_unittest_component`
and `fuchsia_test_component`.

When migrating a unit test whose GN target was a `fuchsia_unittest_component`
wrapping the test binary (such as `//src/lib/cobalt:fuchsia_cobalt_builders_lib_test`),
converting it to `fx_unittest_package` in `BUILD.bazel` and replacing the GN target
with a `bazel_test_suite` of the same name in `BUILD.gn` triggered a false-positive
`gn_target_type_changed` error because `gn_target_type_parity.sh` did not recognize
`fuchsia_unittest_component` as an allowed GN test template that can be converted
to `bazel_test_suite`.

## Remediation

Added `fuchsia_unittest_component` and `fuchsia_test_component` to `GN_TEST_TEMPLATES` in
`checks/gn_target_type_parity.sh`.
