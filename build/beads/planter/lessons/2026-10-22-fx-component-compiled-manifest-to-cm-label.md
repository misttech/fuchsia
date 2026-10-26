# Fx component compiled manifest to cm label attribute alignment parity

- **Learned from:** inner loop friction on a local task (task `swd-pkg-null-update-checker`)
- **Date:** 2026-10-22
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`

## Root cause

When a GN fuchsia_component with cm_label or manifest is migrated to a Bazel
fx_component or fx_test_component using compiled_manifest (which bazel2gn
translates back to cm_label in BUILD.gn), gn_attr_parity only recorded INFO
coverage for the fuchsia_* to fx_* rule mapping against the base BUILD.gn
without recording INFO attribute_alignment coverage for cm_label/manifest ->
compiled_manifest, and migration_sanity only matched fuchsia_* -> fx_* rule
mappings against pre-sentinel targets rather than post-sentinel or deleted GN
package/manifest targets. Additionally, prompts/coder.md stated that bazel2gn
does not convert fx_component or fx_component_manifest.

## Why not a one-off fix

Suppressing the warning for a single package directory would leave every future
component and package migration that maps GN fuchsia_component(cm_label) to
Bazel fx_component(compiled_manifest) or syncs
fx_component/fx_component_manifest below the bazel2gn sentinel vulnerable to
false-positive attribute_alignment warnings from diffengine.
