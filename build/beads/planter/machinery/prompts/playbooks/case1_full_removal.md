# Playbook: Case 1 Full BUILD.gn Removal

Use when nothing GN-only has to stay in the package (e.g. a pure `fuchsia_package` /
`fuchsia_package_with_single_component` directory with no GN callers).
- Convert every GN target to its Bazel equivalent (`fx_package`, `fx_component`,
  `fx_component_manifest`, `fx_packaged_binary`, `resource`, or library/binary rules) and delete
  `BUILD.gn` (`git rm <dir>/BUILD.gn`).
- Tests never force Case 2: every test (C++ or Rust, device or host) migrates to Bazel per
  "Migrating Tests" in the coder prompt, using `fx_unittest_package` / `fx_unittest_component`
  (for unit tests without custom manifests) or explicit `meta/<component>.cml` and
  `fx_test`. They still need a GN `bazel_test_suite` listed by `group("tests")` to reach
  `tests.json`, so such a package keeps a BUILD.gn holding only those two targets (no sentinel,
  no bazel2gn run).
- For assembly packages bridged via `//bundles/assembly/bazel_inputs/<dir>`, switch
  `//bundles/assembly/BUILD.bazel` to `//<dir>:<pkg>`, remove the entry from
  `//bundles/assembly/bazel_inputs/BUILD.gn`, and delete
  `//bundles/assembly/bazel_inputs/<dir>/BUILD.*`.
