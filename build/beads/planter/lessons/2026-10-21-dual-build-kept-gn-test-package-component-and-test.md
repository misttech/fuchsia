# Dual build kept gn test package component and test binary parity

- **Learned from:** inner loop friction on CL If2ee09fc873ef597f74c3f851d001680998678d2, patchset 4 (task `pkg-dns-lib`)
- **Date:** 2026-10-21
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`

## Root cause

When a GN test package and its test component must remain above the BAZEL2GN
SENTINEL in BUILD.gn due to unmigrated or GN-only package dependencies while the
underlying rustc_test/go_test binary is Bazel-buildable, checks/manifest.json
still advertised requiring fx_test_component to be dual-built with #
@bazel2gn:skip in BUILD.bazel, and migration_sanity did not suppress conflicting
generic test-above-sentinel findings or catch # @bazel2gn:skip on a Bazel
rustc_test/go_test whose GN definition was removed from above the sentinel while
still depended upon by a kept GN test component.

## Why not a one-off fix

Fixing only a single package directory leaves checks/manifest.json and
migration_sanity out of sync across all future dual-build migrations where a
kept GN test package and test component above the sentinel consume a
Bazel-buildable rustc_test or go_test binary generated below the sentinel via fx
bazel2gn.
