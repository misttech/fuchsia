# FIDL Migration CL Review Checklist

This checklist defines the target-specific review criteria and standards for code changes migrating FIDL libraries under `//sdk/fidl` (and external FIDL libraries) from GN to Bazel.

---

## 1. Rule Imports & Rule Definitions

* [ ] **FIDL Rule Import:** In `BUILD.bazel`, the `fidl_library` rule is loaded from `//build/bazel/rules/fidl:fidl_library.bzl`:
  ```bazel
  load("//build/bazel/rules/fidl:fidl_library.bzl", "fidl_library")
  ```

---

## 2. Attribute Mapping & Parity

* [ ] **Attribute Mapping:** All attributes in `BUILD.gn` (except `api = "{api_name}"` where `{api_name}` matches `{target_name}.api`) are mapped to `BUILD.bazel` according to `references/gn_to_bazel_attributes_mapping.md`.
* [ ] **SDK Area Mapping:** `sdk_area` in `BUILD.gn` is mapped to `api_area` in `BUILD.bazel`.
* [ ] **SDK Category Mapping:** `sdk_category` in `BUILD.gn` is mapped to `category` in `BUILD.bazel`.
* [ ] **Value Equality:** All mapped attribute values across `BUILD.gn` and `BUILD.bazel` are identical.
* [ ] **No Extra Attributes:** Extra attributes (apart from `visibility`) are not added to `BUILD.bazel` if not present in `BUILD.gn`.

---

## 3. IDK Atom Categorization & Graph Registration

* [ ] **SDK Categorization (`//sdk/fidl/category_lists.bzl`):** If the migrated FIDL target has a `category` attribute, the corresponding IDK atom target (`{target_name}_idk`) is added to the correct category target list in `//sdk/fidl/category_lists.bzl` strictly based on its assigned `category` and `stable` properties (following `idk_atom_target_registration_rules.md`).

---

## 4. bazel2gn Sync Verification

* [ ] **bazel2gn Sync Verification:** The synced `BUILD.gn` (containing the BAZEL2GN SENTINEL) passes the bazel2gn sync check (Verify step of the `syncing-bazel-to-gn` skill).

---

## 5. GN Target Cleanup & Synchronization

* [ ] **Legacy Target Removal:** The legacy `fidl(...)` target and `import("//build/fidl/fidl.gni")` statement are removed from `BUILD.gn`.
* [ ] **Sync Back to GN:** The FIDL target is synchronized back from Bazel to GN using `bazel2gn` (`syncing-bazel-to-gn` skill).

---

## 6. Verification & Build Checks

* [ ] **Bazel Target Build:** The migrated target compiles directly with Bazel:
  ```bash
  fx bazel build --config=fuchsia_platform //sdk/fidl/{library_name}:{library_name}
  ```
* [ ] **FIDL Compatibility Tests:** Compatibility tests pass in both GN and Bazel:
  ```bash
  fx build //sdk/fidl:compatibility_tests
  fx bazel build --config=fuchsia_platform //sdk/fidl:compatibility_tests
  ```
* [ ] **Full Build (when category is unset):** If the migrated FIDL library does NOT specify a `category` attribute, a full platform build passes:
  ```bash
  fx build
  ```
