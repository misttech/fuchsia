# Bazel dependency package reachability in cq tests json query

- **Learned from:** cq failure on CL I7938a093b025d82103ee81db30fab26c7b4ba636, patchset 3 (task `src-developer-ffx-lib-mdns-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`, `checks/manifest.json`

## Root cause

When a migrated package exports a Bazel test via bazel_test_suite, GN gen runs
bazel cquery to populate tests.json and fails with 'no such package' if any
referenced main-repo Bazel dependency package lacks a BUILD.bazel or BUILD file
in the checkout or Gerrit relation chain.

## Why not a one-off fix

Fixing a missing or unstacked Bazel dependency package in one migration CL does
not prevent future migrations with unmigrated or stacked dependencies from
failing CQ tests.json bazel query across other packages.
