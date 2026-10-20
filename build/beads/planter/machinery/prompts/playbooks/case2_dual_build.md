# Playbook: Case 2 Dual-Build (bazel2gn)

Use when the package keeps a BUILD.gn (it has GN-only constructs such as `bazel_test_suite`,
`group("tests")`, a test package Bazel cannot express yet, or GN dependents that must keep
working).
- For `bazel2gn`-convertible non-test targets (libraries, standalone binaries, FIDL),
  BUILD.bazel is the source of truth; never hand-edit BUILD.gn below `## BAZEL2GN SENTINEL` and
  run `fx bazel2gn -d <dir>` after every BUILD.bazel edit.
- Non-test `fuchsia_package`/`fuchsia_package_with_single_component`/`fuchsia_component` targets
  migrate to `fx_package`/`fx_component`/`fx_component_manifest`/`fx_packaged_binary` in
  BUILD.bazel and are deleted from BUILD.gn (`bazel2gn` does not convert them; omit the sentinel
  if BUILD.bazel only defines `fx_*` targets).
- Tests follow "Migrating Tests" in the coder prompt: they live only in BUILD.bazel
  (`fx_unittest_package`, `fx_unittest_component`, explicit `meta/<component>.cml` +
  `fx_packaged_binary` + `fx_component_manifest` + `fx_test_component` +
  `fx_package(test_components = ...)` + `fx_test`, or a host test rule), every test target
  (including `host_*_test` and `wrap_host_rust_test`) and any `with_unit_tests`/`test_deps`
  attributes carry `# @bazel2gn:skip`, and a hand-written `bazel_test_suite` above the sentinel
  that `group("tests")` lists exports them. This applies to Rust unit tests as well as C++ tests.
