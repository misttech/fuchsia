# Missing bazel dependency package in cq cquery

- **Learned from:** cq failure on CL I7938a093b025d82103ee81db30fab26c7b4ba636, patchset 1 (task `src-developer-ffx-lib-mdns-discovery`)
- **Date:** 2026-10-23
- **Changed:** `checks/cq_reachability.sh`

## Root cause

When GN generates tests.json in CQ, it runs bazel cquery across all
bazel_test_suite targets in the GN graph. If a migrated BUILD.bazel target
references a main-repository //package:target dependency whose directory has a
BUILD.gn but no BUILD.bazel or BUILD file in the checkout (or whose stacked
parent migration commit was omitted from the branch ancestry), Bazel analysis
aborts with 'no such package: BUILD file not found'. Static checks did not
previously verify that every main-repository package referenced in dependency
attributes of a migrated BUILD.bazel resolves to an existing Bazel package in
the checkout.

## Why not a one-off fix

Fixing or re-stacking only the single target directory does not prevent future
migrations from referencing unmigrated GN-only packages in BUILD.bazel or
accidentally dropping stacked dependency commits before CQ runs bazel cquery;
enforcing package existence in cq_reachability.sh catches missing Bazel
dependency packages across all migrated directories.
