# Visibility audit false-positive rdep classification: GN template list attributes named target tests/host tests treated as visibility allowlists outside bazel test suite

- **Learned from:** inner loop friction on a local task (task `b2-ex2`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`

## Root cause

visibility_audit stripped every GN `target_tests = [...]` / `host_tests = [...]`
list from caller BUILD.gn files before computing reverse dependencies, assuming
those attributes only occur on bazel_test_suite() (Bazel labels the bridge
builds as top-level targets). Other GN test-group templates use the same
attribute names as real dependency lists, so a GN caller that depends on the
migrated target through such a template was classified as a visibility-only
reference and its required visibility entry was flagged as
visibility_allowlist_false_rdep, while find_rdeps (and the reviewer seats
relying on it) correctly counted it as an rdep, producing a contradictory loop.

## Why not a one-off fix

Any dual-build migration whose GN test package is consumed by a product-bundle
or other test-group template via target_tests/host_tests would hit the same
contradiction between visibility_audit and find_rdeps/reviewer seats; widening
the rust example's visibility by hand would not fix the misclassification for
the next package. Scoping the attribute stripping to bazel_test_suite() blocks
in the shared check fixes the class for all callers.
