# Playbook: Case 2 Dual-Build (bazel2gn)

Use when the package keeps a BUILD.gn (it has GN-only constructs such as GN test copies in
`group("tests")`, a test package Bazel cannot express yet, or GN dependents that must keep
working).
- For `bazel2gn`-convertible non-test targets (libraries, standalone binaries, FIDL),
  BUILD.bazel is the source of truth; never hand-edit BUILD.gn below `## BAZEL2GN SENTINEL` and
  run `fx bazel2gn -d <dir>` after every BUILD.bazel edit.
- Convertible targets with GN-only attributes (`exclude_toolchain_tags`, a `concurrent_jobs` pool,
  toolchain-qualified `assert_no_deps`/`deps` labels, `rustenv`/`inputs` that embed a generated
  file, `configs -=`) are generated too: name those GN lines on the BUILD.bazel target as in
  "GN-only attributes on generated targets" in the coder prompt (e.g. the core
  `component_manager` binary). Keeping such a target hand-written above the sentinel with a
  `# @bazel2gn:skip` Bazel twin is the fallback when no annotation or tag can express it.
- Non-test `fuchsia_package`/`fuchsia_package_with_single_component` targets migrate to
  `fx_package`/`fx_component`/`fx_component_manifest`/`fx_packaged_binary` in BUILD.bazel and are
  deleted from BUILD.gn (`bazel2gn` does not convert non-test `fx_package`; omit the sentinel if
  BUILD.bazel only defines Bazel-only package/test targets). Standalone `fuchsia_component` and
  `fuchsia_component_manifest` targets that still have GN consumers DO convert via `bazel2gn` from
  `fx_component` and `fx_component_manifest` below the sentinel.
- Tests follow "Migrating Tests" in the coder prompt: they are defined in BUILD.bazel
  (`fx_unittest_package`, `fx_unittest_component`, or explicit `meta/<component>.cml`, `fx_packaged_binary` +
  `fx_component_manifest` + `fx_test_component` + `fx_package(test_components = ...)` + `fx_test`,
  or a host test rule) without `# @bazel2gn:skip`, so bazel2gn generates their GN copies and the
  tests run in both build systems while the library has GN users
  (docs/development/build/bazel_concepts/tests.md, "Testing libraries in both build systems").
  Never put `# @bazel2gn:skip` on a test rule or test attribute (`with_unit_tests`,
  `with_host_unit_tests`, `test_deps`) unless one of the conditions in "Migrating Tests"
  holds (target under test is itself skipped in BUILD.bazel, `fx_unittest_package` has `resources`,
  host test uses `data`/`test_data`/`test_label`, the test binary rule is unsupported by
  bazel2gn, or the dual-testing cost exception marked
  `# Needs reviewer attention (bazel2gn skip): ...` alongside `Skipped-Dual-Tests: //<dir>:<target>`
  and `Bug: <id>` for a human to confirm with `Test-Drop-Verified +1`).
  Explicit `fx_package(test_components = ...)` chains (used for custom `.cml`
  tests) are translated by bazel2gn into
  `fuchsia_test_package("<fx_package name>")` (package `<package_name>-gn`)
  plus its `fuchsia_test_component`s and manifests, so they need no hand-written
  GN copy. Only test packages bazel2gn rejects (`fx_unittest_package` with
  `resources`, or a test `fx_package` with `resources`/`tools`/custom
  `fx_test_environment`s) get `# @bazel2gn:skip` on the package and a
  hand-written GN package above the sentinel (`<pkg>-gn`) wrapping the
  generated test binary. `group("tests")` lists every GN copy (host ones as
  `:<name>($host_toolchain)`). Export the Bazel tests per "Export" in the coder
  prompt: through the existing ancestor area's `test_suite()` in BUILD.bazel
  when one exists (setting `visibility = ["//<ancestor>:__pkg__"]` on the test
  target and omitting a per-package `bazel_test_suite`), otherwise through a
  hand-written `bazel_test_suite("bazel_tests")` above the sentinel listed in
  `group("tests")`. This applies to Rust unit tests as well as C++
  and Go tests (for Go, move `*_test.go` files from GN `go_library.sources` into
  `host_go_test`/`go_test` `srcs` with `embed = [":<lib>"]`).
