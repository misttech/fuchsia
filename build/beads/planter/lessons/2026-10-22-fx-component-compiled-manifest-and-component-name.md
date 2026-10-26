# Fx component compiled manifest and component name attribute alignment

- **Learned from:** inner loop friction on a local task (task `swd-pkg-system-update-checker`)
- **Date:** 2026-10-22
- **Changed:** `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When migrating a GN fuchsia_component or fuchsia_test_component that specifies
cm_label (inheriting component_name from the referenced
fuchsia_component_manifest) to a Bazel fx_component or fx_test_component, Bazel
uses compiled_manifest as the equivalent of cm_label and requires explicit
component_name when the Bazel target name differs from the component output
name. diffengine's review_diff attribute_alignment check did not recognize
compiled_manifest as the Bazel counterpart of GN cm_label or explicit
component_name on fx_component/fx_test_component, and migration_sanity.sh /
run_checks.sh only recorded INFO coverage for component_name mismatches between
two explicit component_name values rather than added component_name and
cm_label->compiled_manifest mappings.

## Why not a one-off fix

Suppressing the warning for a single package directory would leave every future
Fuchsia component migration that uses fuchsia_component(cm_label = ...) and
fx_component(compiled_manifest = ..., component_name = ...) blocked by the same
false-positive review_diff/attribute_alignment warning. Extending
migration_sanity.sh and run_checks.sh to recognize compiled_manifest and
component_name on fx_component and fx_test_component resolves this across all
packages.
