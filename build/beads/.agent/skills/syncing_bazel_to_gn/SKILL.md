---
name: syncing-bazel-to-gn
description: Syncing Bazel targets to GN with bazel2gn
---

# Syncing Bazel Targets to GN

This process is necessary if you've migrated a target that is still referenced
by other GN targets, so you can't delete the migrated GN target immediately.

Common use cases include migrating a **library** target that is still referenced
by other binary or test targets in GN.

**NOTE:** Host tool targets are binary targets, not library targets. It is very
rare that you need to automatically sync host tool targets to GN with
`bazel2gn`.

## Steps

Use [`bazel2gn`](../../../../../tools/bazel2gn/README.md) to automatically sync
the Bazel targets to GN:

1.  **Sync targets:**

    Run the following command to sync targets defined in
    `path/to/dir/BUILD.bazel` to `path/to/dir/BUILD.gn`:

    ```bash
    fx bazel2gn -d path/to/dir
    ```

2.  **Clean up GN:**

    Remove the old GN targets you've migrated from `path/to/dir/BUILD.gn`.

3.  **Verify:**

    Confirm your target sync is successful by running:

    ```bash
    fx bazel2gn
    fx host-tool shac check --only verify_bazel2gn
    ```

    `fx bazel2gn` with no `-d` flags re-syncs every directory whose `BUILD.gn`
    contains the BAZEL2GN SENTINEL and should produce no diff; the shac check
    verifies the same without modifying files.

## Shared Lists

If several targets share a list of deps or sources through a top-level variable,
annotate the variable with `# @bazel2gn:transformer=deps` or `#
@bazel2gn:transformer=file_paths`. Without it, `bazel2gn` copies the list
verbatim and skips the label and path rewrites it applies to target attributes.
Add the annotation even if the list doesn't need rewriting today, so a later dep
that does need it is still translated. See `@bazel2gn:transformer` in the
[`bazel2gn` README](../../../../../tools/bazel2gn/README.md).

## Third-Party Dependency Translation

`bazel2gn` automatically translates Bazel external repository targets (such as
`@com_google_googletest//:gtest`, `@re2//:re2`, `@boringssl//:crypto`) to their
equivalent GN labels according to
[`//build/tools/bazel2gn/third_party_target_map.json`](../../../../../tools/bazel2gn/third_party_target_map.json).

If an external repository dependency (starting with `@`) is not in this mapping,
`bazel2gn` will print a warning to `stderr` and append an inline `#
BAZEL2GN_WARNING: Unknown Bazel repository name` comment to the generated GN
line.
