# Dual build private testonly helper target bazel2gn skip

- **Learned from:** inner loop friction on [CL 1885983](https://fuchsia-review.googlesource.com/c/fuchsia/+/1885983), patchset 4 (task `pkg-mock-omaha-server-fuchsia`)
- **Date:** 2026-10-15
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

checks/migration_sanity.sh unconditionally treated every convertible rule in
CONVERTIBLE_BAZEL_RULES (including fidl_library and library rules) annotated
with # @bazel2gn:skip in a dual-build package as a skipped_convertible_target
error, without checking whether the target is a package-private testonly = True
helper whose only consumers are Bazel-only test targets in the same package and
therefore would produce an orphaned target in BUILD.gn if translated by
bazel2gn.

## Why not a one-off fix

Any dual-build package whose unit or integration tests move to Bazel can contain
package-private testonly = True helper targets (such as a test-only
fidl_library, rustc_library, or fx_cc_library) consumed solely by those migrated
Bazel tests; checking the general AST pattern (testonly = True, package-private
visibility, no BUILD.gn or non-test Bazel consumers) prevents both
false-positive skipped_convertible_target errors and orphaned bazel2gn-generated
GN targets across the entire tree.
