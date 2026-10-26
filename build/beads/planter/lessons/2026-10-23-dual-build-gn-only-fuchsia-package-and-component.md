# Dual build gn only fuchsia package and component visibility parity

- **Learned from:** inner loop friction on a local task (task `src_sys_time_testing_dev_time_source`)
- **Date:** 2026-10-23
- **Changed:** `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

Because bazel2gn converts fx_component and fx_component_manifest below the
sentinel but does not convert fx_package, a fuchsia_package whose only consumers
are GN subpackages/deps callers (or whose packaged rustc_binary transitively
depends on a rustc_dylib, causing _build_fuchsia_package_impl to fail on
duplicate direct .so and _solib runfiles) must remain hand-written above the
BAZEL2GN SENTINEL in BUILD.gn without fx_package in BUILD.bazel. Without an
authoritative INFO finding in migration_sanity.sh and explicit guidance in
prompts/coder.md, review_diff and reviewers flagged the retained GN
fuchsia_package and instructed adding fx_package with GN-caller visibility that
fails both visibility_audit and Bazel package assembly.

## Why not a one-off fix

Fixing only a single package directory would leave every other dual-build
component/package migration vulnerable to the same contradiction whenever a
fuchsia_package is consumed only by GN subpackages or wraps a rustc_binary with
transitive rustc_dylib dependencies.
