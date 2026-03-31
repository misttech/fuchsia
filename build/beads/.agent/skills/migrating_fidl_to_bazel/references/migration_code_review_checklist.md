---
name: GN-to-Bazel-Migration-Code-Review-Checklist
description: >
  This is a checklist for code reviews of GN to Bazel migrations.
---

# GN to Bazel Migration Code Review Checklist

## General

* [ ] The copyright year in the new generated BUILD.bazel file is the current year.
* [ ] The copyright header in the BUILD.gn file is not changed after the migration.
* [ ] The comment above or at the same line with the attribute in the BUILD.gn file is copied to the same position related to the mapped attribute in the BUILD.bazel file.
* [ ] The fidl_library is loaded from `//build/bazel/rules/fidl:fidl_library.bzl`.
* [ ] Except those in exception_list, all the attributes in the BUILD.gn are migrated to the BUILD.bazel file.
  The migration mappings are as follows:
  - `sources` -> `srcs`
  - `public_deps` -> `deps`
  - `sdk_area` -> `api_area`
  - `sdk_category` -> `category`
  - `enable_* = true` -> `enable_* = True`
  - `visibility = ["*"]` -> `visibility = ["//visibility:public"]`
  *(For other values of `visibility`, map to the corresponding visibility in
  Bazel).*
  The exception_list is as follows:
  - `api`
* [ ] The `fidl` target is included in the correct list in `//sdk/fidl/BUILD.bazel` based on its category:
  - `partner` (and `stable` is `true`) -> `_partner_idk_stable_fidl_libraries_targets_list`
  - `partner` (and `stable` is `false`) -> `_partner_idk_unstable_fidl_libraries_targets_list`
  - `prebuilt` -> `_prebuilt_fidl_libraries_targets_list`
  - `host_tool` -> `_host_tool_fidl_libraries_targets_list`
  - `compat_test` -> `_compat_test_fidl_libraries_targets_list`
* [ ] When adding the target to a categorized list, you must also add the
    library's package (e.g., `"//sdk/fidl/{library_name}:__pkg__"`) to the
    `visibility` list of the corresponding `filegroup` allowlist in
    `//sdk/fidl/BUILD.bazel`. The allowlists share similar names to the lists
    above (e.g., `partner_idk_fidl_library_allowlist`).
* [ ] The label of the verification target `//path/to/dir:verify_bazel2gn` is added to the
    `deps` of `//build:bazel2gn_verifications` in `//build/BUILD.gn`.
