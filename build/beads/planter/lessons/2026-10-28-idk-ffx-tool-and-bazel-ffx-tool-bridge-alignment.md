# Idk ffx tool and bazel ffx tool bridge alignment

- **Learned from:** inner loop friction on CL I7f36942e761013954c627605b4d9f48069cdb0ed, patchset 3 (task `src-developer-ffx-plugins-coverage`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`, `tools/run_checks.sh`

## Root cause

When ffx_tool became Bazel-only (noGNEquivalent in bazel2gn) and idk_ffx_tool
was introduced for partner IDK ffx subtools, migration_sanity.sh only emitted
INFO sentinel_drift coverage for sentinel-less BUILD.gn files when BUILD.bazel
had no unskipped convertible targets, missing packages whose internal helper
rustc_library/host_rustc_test targets have zero GN consumers and whose BUILD.gn
retains only a bazel_ffx_tool bridge. In addition, gn_attr_parity.sh and
run_checks.sh lacked attribute_alignment coverage for the ffx_tool ->
idk_ffx_tool rule mapping, GN sdk_target_name replaced by idk_ffx_tool's
generated <name>_idk target (via idk_name/category), and idk_name matching GN's
explicit output_name when output_name differs from target_name.

## Why not a one-off fix

Every partner IDK ffx plugin or subtool migrated from GN ffx_tool to Bazel
idk_ffx_tool with a GN bazel_ffx_tool bridge and Bazel-only helper libraries
triggers the exact same review_diff sentinel_drift and attribute_alignment
false-positive warnings unless the static checks recognize the general rule and
attribute mappings.
