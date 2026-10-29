# Structured Configuration Internal FIDL Validation Exclusions and Component/FIDL Attribute Alignment Parity

- **Learned from:** inner loop friction on CL I6c67c92e89847a6089233b3a242d32d0da29000f, patchset 1 (task `netemul-test-support`)
- **Date:** 2026-10-26
- **Changed:** `checks/build_verification.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

First, build_verification's validate_json_exclusions only excluded
<name>_validate_ir_json for direct fidl_library/fidl_ir calls, missing
<name>_fidl_internal_validate_ir_json expanded inside
fuchsia_structured_config_rust_lib symbolic macros, causing
assert_no_deps_aspect to crash on _validate_json_action's scalar data attribute
during :all builds. Second, gn_attr_parity's attribute_alignment coverage only
checked literal output_name in GN and did not account for diffengine aliasing GN
`name` on fidl() and fuchsia_structured_config_*() to output_name (mapped to
library_name and crate_name in Bazel) or mapping GN cm_label on
fuchsia_component to compiled_manifest on fx_component.

## Why not a one-off fix

Any package defining a fuchsia_structured_config_rust_lib target, a fidl()
target with an explicit library name, or a fuchsia_component() target using
cm_label hits the exact same top-level :all aspect crash in build_verification
and false-positive attribute_alignment warnings in review_diff unless handled
generically across all packages.
