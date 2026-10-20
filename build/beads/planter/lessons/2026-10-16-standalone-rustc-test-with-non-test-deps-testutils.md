# Standalone rustc test with non test deps testutils swap, single-use private variable cleanup, and rustc library with features host test naming

- **Learned from:** human review on CL Ia917b210ccfb0ba163984e259811e3cdbb419d16, patchset 2 (task `netstack3-base`)
- **Date:** 2026-10-16
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

The check_rustc_test_crate_reuse check in bazel_minimality unconditionally
rejected any unit-test rustc_test with srcs and no crate attribute whenever a
matching rustc_library existed in the package, even when GN used non_test_deps
to swap a dependency for its testutils variant (where attaching to the library
via crate inherits conflicting non-testutils deps and attaching to the testutils
library enables the testutils feature on the crate under test). Additionally,
bazel_minimality did not require an explanatory comment above such standalone
rustc_test targets, did not flag single-use private Starlark variables left over
from workarounds, and did not check that wrap_host_rust_test uses the
<base>-host-test naming convention in rustc_library_with_features packages.

## Why not a one-off fix

Fixing only the current package would leave bazel_minimality rejecting valid
standalone rustc_test targets across all other rustc_library_with_features
packages that swap dependencies via non_test_deps and test_deps, and would not
prevent unannotated standalone rustc_test targets, single-use private variables,
or inconsistent wrap_host_rust_test names in future migrations.
