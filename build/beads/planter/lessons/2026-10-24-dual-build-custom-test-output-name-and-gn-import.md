# Dual build custom test output name and gn import hygiene

- **Learned from:** human review on CL I8de6659b6ab112b3b500c35f464234d271450ed2, patchset 1 (task `src-connectivity-overnet-lib-core`)
- **Date:** 2026-10-24
- **Changed:** `checks/bazel_minimality.sh`, `checks/gn_attr_parity.sh`, `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

First, checks/bazel_minimality.sh unconditionally rejected crate_name on any
rustc_test with crate = ':<lib>' or a matching library, while
checks/gn_attr_parity.sh did not verify that a custom GN
name/output_name/crate_name at the change base is preserved on the
bazel2gn-generated target and had a sentinel-splitting bug ('@bazel2gn:skip'
instead of 'BAZEL2GN SENTINEL') in check_rust_test_output_name_collision.
Second, checks/migration_sanity.sh did not check for leftover .gni imports above
the BAZEL2GN SENTINEL whose templates are no longer used above the sentinel and
are already imported below the sentinel by
//build/tools/bazel2gn/bazel_migration.gni.

## Why not a one-off fix

Fixing only a single package leaves all future dual-build migrations vulnerable
to dropping custom GN binary/test output names (causing root_out_dir binary
collisions across packages with generic target names like core or lib) and
leaving dead .gni imports above the BAZEL2GN SENTINEL.
