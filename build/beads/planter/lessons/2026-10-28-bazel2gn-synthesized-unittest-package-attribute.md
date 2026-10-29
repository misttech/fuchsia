# Bazel2gn synthesized unittest package attribute alignment

- **Learned from:** inner loop friction on a local task (task `b3-sdkf1`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`

## Root cause

When migrating a GN fuchsia_unittest_package that did not set component_name or
package_name at the change base to fx_unittest_package in BUILD.bazel, bazel2gn
synthesizes package_name = "<name>-gn" and component_name = "<name>" on the
generated fuchsia_unittest_package below the sentinel. Planter's diffengine
compares the generated GN target against BUILD.bazel and flags component_name in
GN not carried over to Bazel, while gn_attr_parity.sh only checked base gn_top
(before the migration) rather than cur_top (below the sentinel) and
migration_sanity.sh did not emit the deduplicating INFO attribute_alignment
finding for fx_unittest_package / fx_unittest_component.

## Why not a one-off fix

Any dual-built unit test package migrated from fuchsia_unittest_package to
fx_unittest_package without an explicit component_name in the original BUILD.gn
receives a bazel2gn-synthesized component_name = "<name>" below the sentinel,
triggering the same false-positive review_diff attribute_alignment warning
unless the check machinery records INFO coverage for bazel2gn-synthesized
unittest package and packaged-binary attributes across all packages.
