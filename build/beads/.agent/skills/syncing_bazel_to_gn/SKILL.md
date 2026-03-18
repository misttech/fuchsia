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

3.  **Add verification:**

    Add `//path/to/dir:verify_bazel2gn` to the `deps` of
    `//build:bazel2gn_verifications` in `//build/BUILD.gn`.

4.  **Verify:**

    Confirm your target sync is successful by running:

    ```bash
    fx build --host //build:bazel2gn_verifications
    ```
