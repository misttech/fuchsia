# Rust crate name normalization and data deps alignment parity

- **Learned from:** inner loop friction on a local task (task `src-diagnostics-triage`)
- **Date:** 2026-10-27
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

The gn_attr_parity check did not emit non-blocking INFO alignment coverage when
a GN Rust target's hyphenated name/output_name mapped to an
underscore-normalized Bazel crate_name/binary_name (which diffengine reports as
`output_name ([...]) != crate_name ([...])` on the attribute line) or when GN
data_deps and top-level GN list variables were preserved via
`@bazel2gn:raw_attribute:data_deps` and companion host_test_data_files /
wrap_host_rust_test test_data targets.

## Why not a one-off fix

Many Fuchsia Rust libraries and host integration tests define hyphenated GN
crate names or runtime binary dependencies in data_deps alongside shared
top-level GN dependency lists; extending gn_attr_parity to verify
hyphen-to-underscore crate_name mappings and data_deps / test_data coverage
eliminates false-positive review_diff warnings across all such migrations.
