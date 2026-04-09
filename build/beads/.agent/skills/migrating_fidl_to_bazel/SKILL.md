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

3.  Define the equivalent Bazel target for the FIDL library. Use `fidl_library`
    loaded from `//build/bazel/rules/fidl:fidl_library.bzl`.

    > [!IMPORTANT]
    > **Preserve Comments:** Except the copyright header, you MUST copy all
    comments from the `BUILD.gn` file to the `BUILD.bazel` file. If a comment
    is above or on the same line as an attribute in `BUILD.gn`, it should be
    placed in the same relative position to the mapped attribute in `BUILD.bazel`.

    Map the attributes as follows:
    -   `sources` -> `srcs`
    -   `public_deps` -> `deps`
    -   `sdk_area` -> `api_area`
    -   `sdk_category` -> `category`
    -   `enable_* = true` -> `enable_* = True`
    -   `visibility = ["*"]` -> `visibility = ["//visibility:public"]`
    *(For other values of `visibility`, map to the corresponding visibility
    in Bazel).*
    *(Example: If a comment is above `excluded_checks = [`, it should sit directly
    above `excluded_checks = [` in the `BUILD.bazel` file).*
4.  If dependencies are missing Bazel targets, migrate those dependencies first.
5.  Verify the Bazel target builds successfully:

    ```bash
    fx bazel build --config=fuchsia //sdk/fidl/{library_name}:{library_name}
    ```

## 2. Register the target

1.  If Bazel target has a category, add it to the appropriate list in
    `//sdk/fidl/BUILD.bazel` based on its category:
    -   `partner` (and `stable` is `true`) ->
        `_partner_idk_stable_fidl_libraries_targets_list`
    -   `partner` (and `stable` is `false`) ->
        `_partner_idk_unstable_fidl_libraries_targets_list`
    -   `prebuilt` -> `_prebuilt_fidl_libraries_targets_list`
    -   `host_tool` -> `_host_tool_fidl_libraries_targets_list`
    -   `compat_test` -> `_compat_test_fidl_libraries_targets_list`

## 3. Sync FIDL targets back to GN

1.  Remove the `fidl(...)` target and the `import("//build/fidl/fidl.gni")`
    statement from the `BUILD.gn` file.
2.  Sync the target back from Bazel to GN using the `syncing-bazel-to-gn` skill
    (see `../syncing_bazel_to_gn/SKILL.md`).

## 4. Review, Verification and Cleanup

1.  **Review the changed code** using the checklist in
    `references/migration_code_review_checklist.md`.

2.  Format your changes:

    ```bash
    fx format-code --parallel
    ```

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
