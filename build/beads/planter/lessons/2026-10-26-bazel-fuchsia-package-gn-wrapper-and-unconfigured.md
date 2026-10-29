# Bazel fuchsia package gn wrapper and unconfigured gn test footer labels

- **Learned from:** inner loop friction on CL Ie9d651577c24f541db3b76fe7c4d99578fa4d076, patchset 2 (task `fake-netstack`)
- **Date:** 2026-10-26
- **Changed:** `checks/build_verification.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`

## Root cause

First, `prompts/coder.md` and `prompts/playbooks/case2_dual_build.md` instructed
deleting migrated `fuchsia_package` and `fuchsia_package_with_single_component`
targets from `BUILD.gn` without documenting that packages still referenced by GN
callers (e.g. via `subpackages`) must be wrapped above the sentinel with
`bazel_fuchsia_package("<pkg>")`. Second, `checks/build_verification.sh` only
checked `:tests` when reporting `dependents_note` for unconfigured GN targets in
`<build dir>/ninja_outputs.json` and skipped emitting that note on
`--skip-build` or reused build runs, so the coder received no warning against
recording `fx build //<dir>:<pkg>` for unconfigured `bazel_fuchsia_package` or
binary targets in `CoderReport.test_footers` (which subsequently failed
`test_verifier` with `missing_gn_test_target`).

## Why not a one-off fix

Fixing only a single benchmark package directory would leave future package
migrations with GN `subpackages` or `deps` callers vulnerable to either deleting
needed `bazel_fuchsia_package` wrappers or recording unconfigured GN package
labels in `CoderReport.test_footers` that fail `fx gn desc` in `test_verifier`.
