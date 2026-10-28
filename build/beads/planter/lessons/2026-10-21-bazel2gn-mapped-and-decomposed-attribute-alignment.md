# Bazel2gn mapped and decomposed attribute alignment

- **Learned from:** inner loop friction on a local task (task `src_developer_ffx_plugins_package_blob_compress`)
- **Date:** 2026-10-21
- **Changed:** `checks/gn_attr_parity.sh`, `checks/gn_target_type_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

Planter's built-in diffengine only populates its internal Bazel SDKCategory
field from `category` (used by FIDL and IDK rules) rather than `sdk_category`
(used by `ffx_plugin` and `ffx_tool`), does not map `ffxPluginAttrMap`
attributes (`args_srcs` -> `args_sources`, `args_crate_root` ->
`args_source_root`), and combines any such unmapped attribute note with `GN
with_unit_tests decomposed into explicit test target` into a blocking WARNING
`attribute_alignment` finding. Meanwhile, `gn_attr_parity.sh` only recognized
`rustc_test`, `rust_test`, `fx_test`, and `fx_unittest_package` in
`has_bazel_test_rule` (omitting `host_rustc_test` and `wrap_host_rust_test`) and
did not emit INFO `attribute_alignment` coverage for
`ffx_plugin`/`ffx_tool`/`bazel2gn` mapped attributes or attributes already
preserved below `## BAZEL2GN SENTINEL`.

## Why not a one-off fix

Hardcoding a single plugin directory or target name would fail on all other
`ffx_plugin`, `ffx_tool`, and dual-build targets across the tree that use
`sdk_category`, `args_srcs` (`args_sources`), `args_crate_root`
(`args_source_root`), `args_with_unit_tests`, `args_test_deps`, or standalone
`host_rustc_test` unit test decomposition. Verifying mapped Bazel attributes,
`bazel2gn`-generated `BUILD.gn` attributes below the sentinel, and decomposed
unit test targets in `gn_attr_parity.sh`, `gn_target_type_parity.sh`, and
`run_checks.sh` resolves this class of false-positive `attribute_alignment`
warnings treewide.
