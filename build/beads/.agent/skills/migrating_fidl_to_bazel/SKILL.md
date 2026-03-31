---
name: migrating-fidl-to-bazel
description: >-
  Migrates FIDL library targets from the GN build system to Bazel. Use when a
  user asks to migrate a FIDL target under //sdk/fidl to Bazel, create a
  BUILD.bazel file for a FIDL library, or sync a GN FIDL target to Bazel.
---

# Migrating FIDL from GN to Bazel

This skill guides the migration of FIDL library targets under //sdk/fidl from
GN to Bazel.

## 1. Create the Bazel target

1.  Identify the requested `BUILD.gn` files and their `fidl` GN targets based
    on the user request.
2.  Create a `BUILD.bazel` file in the same directory as the `BUILD.gn` file.
    See `assets/copyright_header_template.md` for the copyright header template.
    Add the copyright header to the top of the file.

3.  Define the equivalent Bazel target for the FIDL library. Copy all comments
    from the `BUILD.gn` file. Map the attributes as follows:
    -   `sources` -> `srcs`
    -   `public_deps` -> `deps`
    -   `sdk_area` -> `api_area`
    -   `sdk_category` -> `category`
    -   `enable_* = true` -> `enable_* = True`
    -   `visibility = ["*"]` -> `visibility = ["//visibility:public"]`
    *(For other values of `visibility`, map to the corresponding visibility in
    Bazel).*
4.  If an attribute has a comment just above it or at the same line with it in
    the BUILD.gn, copy the comment to the same position related to the mapped
    attribute in the BUILD.bazel file.
    *(Example: If a comment is above `sources = [`, it should sit directly
    above `srcs = [` in the `BUILD.bazel` file).*
5.  If dependencies are missing Bazel targets, migrate those dependencies first.
6.  Verify the Bazel target builds successfully:

    ```bash
    fx bazel build --config=fuchsia //sdk/fidl/{library_name}:{library_name}
    ```

## 2. Register the target

1.  Add the Bazel target to the appropriate list in `//sdk/fidl/BUILD.bazel`
    based on its category:
    -   `partner` (and `stable` is `true`) ->
        `_partner_idk_stable_fidl_libraries_targets_list`
    -   `partner` (and `stable` is `false`) ->
        `_partner_idk_unstable_fidl_libraries_targets_list`
    -   `prebuilt` -> `_prebuilt_fidl_libraries_targets_list`
    -   `host_tool` -> `_host_tool_fidl_libraries_targets_list`
    -   `compat_test` -> `_compat_test_fidl_libraries_targets_list`

2.  When adding the target to a categorized list, you must also add the
    library's package (e.g., `"//sdk/fidl/{library_name}:__pkg__"`) to the
    `visibility` list of the corresponding `filegroup` allowlist in
    `//sdk/fidl/BUILD.bazel`. The allowlists share similar names to the lists
    above (e.g., `partner_idk_fidl_library_allowlist`).

## 3. Sync FIDL targets back to GN

1.  Remove the `fidl(...)` target, and if the `//build/fidl/fidl.gni`is imported
    in the `//build/tools/bazel2gn/bazel_migration.gni`, remove the `import`
    statement from the `BUILD.gn`, too.
2.  Sync the target back from Bazel to GN using the `syncing-bazel-to-gn` skill
    (see `../syncing_bazel_to_gn/SKILL.md`).

## 4. Verification and Cleanup

1.  Format your changes:

    ```bash
    fx format-code --parallel
    ```

2. Review the changed code with the checklist in
    `references/migration_code_review_checklist.md`.

3.  Verify the build executes correctly for compatibility tests:

    ```bash
    fx set core.x64
    fx build //sdk/fidl:compatibility_tests
    ```

3.  If the migrated FIDL library does NOT have the `category` attribute set, you
    MUST run a full build to verify:

    ```bash
    fx build
    ```

4.  Address and fix any build errors that occur during verification.
