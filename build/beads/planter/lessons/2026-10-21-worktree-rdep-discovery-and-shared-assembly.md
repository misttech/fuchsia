# Worktree Rdep Discovery and Shared Assembly / Packaging Review-Diff Parity

- **Learned from:** inner loop friction on a local task (task `pkg-shutdown-shim`)
- **Date:** 2026-10-21
- **Changed:** `checks/gn_attr_parity.sh`, `checks/migration_sanity.sh`, `tools/find_rdeps.sh`, `tools/run_checks.sh`

## Root cause

First, tools/find_rdeps.sh unconditionally passed '-g !.jiri_root/**' without
'--no-ignore-parent' and treated ripgrep exit code 1 as authoritative, excluding
the entire checkout when executed inside a '.jiri_root/worktrees/<name>'
workspace. Second, migration_sanity.sh, gn_attr_parity.sh, and run_checks.sh did
not record INFO coverage for pre-existing default_visibility in shared
bundles/assembly BUILD.bazel files, fx_package(components = [...]) vs GN
fuchsia_package(deps = [...]), rustc_binary(binary_name, build_flags) vs GN
output_name/configs, or positional component_name pairing across multiple
fuchsia_component targets.

## Why not a one-off fix

Any package migration that removes a bundles/assembly/bazel_inputs bridge,
defines an fx_package with components = [...], sets binary_name and build_flags
on a rustc_binary, or migrates one of several fuchsia_component targets in a
test BUILD.gn inside a worktree will hit the same false-positive rdep and
review_diff findings unless handled systemically in the check and tool scripts.
