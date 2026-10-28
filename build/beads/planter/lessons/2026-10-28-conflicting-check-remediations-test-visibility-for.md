# Conflicting check remediations: test visibility for ancestor test suite wiring vs. reverse-dependency visibility audit

- **Learned from:** inner loop friction on a local task (task `b2-fidl1`)
- **Date:** 2026-10-28
- **Changed:** `checks/cq_reachability.sh`

## Root cause

cq_reachability built its suggested test visibility from the ancestor test_suite
package plus every package whose BUILD.gn lists the test
(direct_gn_test_callers). GN files reference the GN target, and bazel_test_suite
builds the Bazel test as an unchecked top-level target, so those packages are
not Bazel reverse dependencies. visibility_audit correctly rejects them as
false_positive_rdep_visibility, leaving the coder with two checks that
contradict each other.

## Why not a one-off fix

Any migrated test package listed in a parent BUILD.gn group while an ancestor
BUILD.bazel test_suite exists hits the same contradictory remediation, whichever
directory it is in. Editing one BUILD.bazel by hand leaves the check suggesting
the rejected visibility in every later migration, so the remediation logic
itself has to change.
