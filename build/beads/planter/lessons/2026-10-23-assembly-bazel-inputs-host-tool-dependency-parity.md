# Assembly bazel inputs host tool dependency parity

- **Learned from:** cq failure on CL I7b14c333e2ec581df3579baa7cbad537db315676, patchset 1 (task `swd-pkg-null-update-checker`)
- **Date:** 2026-10-23
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When migrating a Fuchsia package wired into //bundles/assembly/BUILD.bazel that
uses a fuchsia_structured_config_* library (which invokes
@gn_targets//toolchain_host_x64/tools/configc:configc), migration_sanity.sh
checked the removal of the legacy //bundles/assembly/bazel_inputs/<dir> bridge
but did not verify that _AIB_TOOLS_DEPS in
//bundles/assembly/bazel_inputs/BUILD.gn exported
//tools/configc:bazel_input($host_toolchain), causing SDK builders that run
//bundles/assembly:platform without //build/bazel/target_tests in their GN graph
to fail with Unknown @gn_targets targets.

## Why not a one-off fix

Fixing only a single package migration leaves future migrations of
structured-config-backed platform packages vulnerable whenever _AIB_TOOLS_DEPS
in //bundles/assembly/bazel_inputs/BUILD.gn is missing the required @gn_targets
host tool input; enforcing this deterministically in migration_sanity.sh catches
the missing tool input across all migrated assembly packages.
