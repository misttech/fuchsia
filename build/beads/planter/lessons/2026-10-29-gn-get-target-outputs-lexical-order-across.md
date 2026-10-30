# Gn get target outputs lexical order across sentinel

- **Learned from:** cq failure on CL I6885d1c5c4ceea8a2e1ddb42c98c15101ddc308b, patchset 1 (task `pkg-tests-pkg-resolver`)
- **Date:** 2026-10-29
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

In GN, get_target_outputs() (invoked directly or via cm_label on
fuchsia_component, fuchsia_test_component, and fuchsia_structured_config_*,
test_components on fuchsia_test_package, or component on fuchsia_test) only
resolves targets previously defined earlier in the current BUILD.gn file.
migration_sanity.sh treated any pre-sentinel fuchsia_component with a
cross-package dependency as GN-blocked even when the dependency was an
fx_packaged_binary or resource target in Bazel, while still forcing its
pre-sentinel fuchsia_component_manifest below the BAZEL2GN SENTINEL and lacking
any static check for forward or missing same-file get_target_outputs()
references. When the migrated package was outside the local args.gn build graph,
local fx build never loaded its BUILD.gn, leaving the fatal GN evaluation error
undetected until CQ loaded the file via a broader build graph.

## Why not a one-off fix

Any dual-build Fuchsia package that splits component manifests, components,
structured config targets, or test packages across the BAZEL2GN SENTINEL can
trigger the same fatal GN get_target_outputs() forward-reference failure on CQ
whenever a pre-sentinel target references a target generated below the sentinel
and local args.gn does not include the package. Enforcing GN
get_target_outputs() lexical ordering statically in migration_sanity.sh and
aligning pre-sentinel component/manifest block detection prevents both the
broken split and false catch-22 check findings across all future migrations.
