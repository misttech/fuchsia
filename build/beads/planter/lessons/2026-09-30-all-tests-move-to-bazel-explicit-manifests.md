# All tests move to Bazel with explicit manifests; bazel2gn does not translate tests

- **Learned from:** team decision on Bazel test migration (tests move to Bazel; GN's generated test manifests are not reimplemented in Bazel) and upstream Rust support in `//build/bazel/rules/packages:fx_packaged_binary.bzl` (bundles `libstd` and its `.build-id`) and `//build/bazel/rules/rust:generate_unit_tests.bzl` (`with_unit_tests = "fuchsia" | "host" | "both"`)
- **Date:** 2026-09-30
- **Supersedes:** the Rust part of `2026-09-29-bazel-device-and-host-test-migration.md`
- **Changed:** `prompts/coder.md`, `prompts/playbooks/case1_full_removal.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/target_parity.md`, `prompts/reviewers/dual_build_sentinel.md`, `checks/migration_sanity.sh`, `checks/cq_reachability.sh`, `checks/build_only_scope.sh`, `checks/gn_target_type_parity.sh`, `checks/build_verification.sh`, `checks/manifest.json`
- **Superseded by:** `2026-10-22-policy-drift-public-visibility-and-test-skip.md` (test skips only)

## Status: partly superseded

The claim that bazel2gn does not translate tests, and the rule to put
`# @bazel2gn:skip` on tests and test attributes in dual-build packages, are
outdated. bazel2gn now generates GN copies of tests, and tests of dual-built
targets run in both build systems by default
(docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems).
The explicit-manifest guidance below still applies where a custom `.cml` is
needed.

## Root cause

The machinery still assumed Bazel could not run Rust tests on a device: it
kept Rust unit tests in GN through bazel2gn (`with_host_unit_tests = True`
becoming GN `with_unit_tests = true`, wrapped by a GN
`fuchsia_unittest_package` above the sentinel), rejected any `.cml` using the
Rust test runner shard, and treated `rustc_test`/`go_test` as targets bazel2gn
must convert. Nothing taught the coder that `fuchsia_unittest_package` and
`fuchsia_unittest_component` generate their manifest from deps metadata
(runner shard from the test framework, `syslog/use.shard.cml`, extra shards
such as `death_test` or `tmp_storage`), which Bazel will not replicate.

## Fix

Every test moves to Bazel: the coder writes `meta/<component>.cml`
reproducing GN's generated manifest and builds the package explicitly
(`fx_packaged_binary` of the C++ binary, the `rustc_*` `:<name>_test`, or a
`rustc_test` -> `fx_component_manifest` -> `fx_test_component` ->
`fx_package(test_components)` -> `fx_test`), exported by `bazel_test_suite`.
In dual-build packages every test target and the `with_unit_tests`/`test_deps`
attributes carry `# @bazel2gn:skip`; `migration_sanity` reports
`test_generated_by_bazel2gn` otherwise, and `cq_reachability` reports a Rust
device test binary that no `fx_packaged_binary` packages.

## Why not a one-off fix

Almost every Rust package has unit tests, so every Rust migration would
otherwise keep a GN test package and bazel2gn-generated tests that the team
has decided to remove.
