# Cc dependency exported header inclusion parity

- **Learned from:** inner loop friction on a local task (task `b2-ex2`)
- **Date:** 2026-10-28
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`

## Root cause

check_undeclared_cc_local_headers in checks/gn_attr_parity.sh checked only the
srcs, hdrs, and textual_hdrs attributes declared directly on each C/C++ target
and ignored headers exported via hdrs, textual_hdrs, or hdrs_for_internal_use of
the target's dependencies (and referenced filegroups), causing a false-positive
undeclared_cc_local_header error whenever a test binary or consumer target in
the same directory included a header exported by a library in its deps.

## Why not a one-off fix

Adding a library dependency's public header to a consumer binary's srcs in a
single package violates 1:1 source parity with GN (triggering
target_parity/source_parity errors) and fails to prevent the same conflict
across all C/C++ packages where binaries or unit tests include local headers
exported by same-package library dependencies.
