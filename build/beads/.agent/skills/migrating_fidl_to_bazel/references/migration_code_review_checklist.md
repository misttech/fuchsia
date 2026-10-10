---
name: GN-to-Bazel-Migration-Code-Review-Checklist
description: >
  This is a checklist for code reviews of GN to Bazel migrations.
---

# GN to Bazel Migration Code Review Checklist

## General

* [ ] The copyright year in the new generated BUILD.bazel file is the current year.
* [ ] The copyright header in the BUILD.gn file is not changed after the migration.
* [ ] The comment above or at the same line with the attribute in the BUILD.gn file before migration is copied to the same position related to the mapped attribute in the new generated BUILD.bazel file.
* [ ] The fidl_library is loaded from `//build/bazel/rules/fidl:fidl_library.bzl`.
* [ ] Except those in exception_list, all the attributes in the BUILD.gn are migrated to the BUILD.bazel file according to the attributes mapping in `references/gn_to_bazel_attributes_mapping.md`.
  The exception_list is as follows:
  - `api`
* [ ] The `fidl` target is included in the correct list in `//sdk/fidl/category_lists.bzl` based on its category according to the rules in `references/idk_atom_target_registration_rules.md`.
* [ ] The synced BUILD.gn passes the bazel2gn sync check (Verify step of the `syncing-bazel-to-gn` skill).
