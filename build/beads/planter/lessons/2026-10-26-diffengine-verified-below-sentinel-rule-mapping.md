# Diffengine verified below sentinel rule mapping coverage

- **Learned from:** inner loop friction on CL I9492e89e2c7d9dbc88feba65ec6760313bdd095c, patchset 2 (task `src_sys_time_httpsdate_time_source`)
- **Date:** 2026-10-26
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`

## Root cause

When diffengine (review_diff) detects an expected GN-to-Bazel rule name
translation (such as fuchsia_component_manifest -> fx_component_manifest, cm ->
fx_compiled_manifest, fuchsia_component -> fx_component, or config ->
build_flags/rustc_check_cfg_build_flags) on a target that bazel2gn also syncs
below the BAZEL2GN SENTINEL in BUILD.gn, diffengine appends the suffix
'(verified: bazel2gn output below the sentinel is <tmpl>("<name>"), the original
GN template)' to the attribute_alignment message. Existing INFO coverage in
checks/migration_sanity.sh and checks/gn_attr_parity.sh emitted only the
unsuffixed 'Unusual rule mapping from <tmpl> to <rule>' string, so exact
(category, file, line, message) deduplication did not match the suffixed
review_diff WARNING.

## Why not a one-off fix

Suppressing the warning for a single target name or package directory would
leave every other dual-build package that migrates fuchsia_*
component/manifest/package rules, cm() manifests, or config() build flags via
bazel2gn vulnerable to the same false-positive review_diff attribute_alignment
warning whenever bazel2gn emits the original GN template below the sentinel.
