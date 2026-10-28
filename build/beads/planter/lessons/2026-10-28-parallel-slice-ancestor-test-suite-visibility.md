# Parallel slice ancestor test suite visibility contradiction

- **Learned from:** inner loop friction on a local task (task `virt-devices-a`)
- **Date:** 2026-10-28
- **Changed:** `checks/visibility_audit.sh`

## Root cause

After a concurrent machinery update resolved the Python signature and symbol
crashes in visibility_audit and cq_reachability, visibility_audit still rejected
package-scoped visibility grants ('//<ancestor>:__pkg__') on migrated Bazel test
targets with false_positive_rdep_visibility (and visibility_widened_beyond_gn)
whenever the ancestor BUILD.bazel test_suite had not yet been updated in the
working tree (such as during parallel multi-package slice migrations where
ancestor BUILD.bazel edits are deferred to outside_changes integration),
directly contradicting cq_reachability requiring that visibility grant.

## Why not a one-off fix

Any multi-package migration split across parallel slice coders that exports leaf
fx_unittest_package, fx_test, or host test targets to an ancestor area
test_suite via deferred outside_changes hits this exact contradiction between
visibility_audit and cq_reachability unless visibility_audit recognizes pending
package-scoped visibility grants to an ancestor test_suite package.
