# Host Tool Migration CL Review Checklist

This checklist defines the review criteria and standards for code changes migrating host tools (Go and Rust host binaries, libraries, and host tests) from GN to Bazel.

---

## 1. Target Platform Compatibility (`target_compatible_with`)

* [ ] **Host-Only Targets:** Host tools, host tests, and libraries guarded by `is_host` in GN MUST set `target_compatible_with = HOST_OS_CONSTRAINTS` (loaded from `//build/bazel/platforms:constraints.bzl`).
  ```bazel
  load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
  ```
* [ ] **Avoid Deprecated Constraints:** Do **NOT** use `HOST_CONSTRAINTS` from `@platforms//host:constraints.bzl`. Refer to `build/beads/references/target_compatible_with.md`.
* [ ] **Platform-Agnostic / Cross-Toolchain Libraries:** Libraries that are not guarded by `is_host` in GN (e.g. cross-toolchain libraries in `//src/developer/ffx/lib/` built for both host and Fuchsia target devices) omit `target_compatible_with`.

---

## 2. Go Host Tools Review Criteria

* [ ] **Rule Imports:**
  - `go_library` loaded from `@io_bazel_rules_go//go:def.bzl`.
  - `go_binary_host_tool` loaded from `//build/bazel/rules/host:defs.bzl` (or `idk_go_binary_host_tool` from `//build/bazel/rules/idk:idk_host_tool.bzl` for IDK tools; do **not** use standard `go_binary`).
  - `host_go_test` loaded from `//build/bazel/rules/host_tests:host_go_test.bzl`.
* [ ] **`importpath` Alignment:** The `importpath` in `go_library` matches the exact package import string used in dependent Go source files (`go.fuchsia.dev/fuchsia/...`).
* [ ] **Strict Dependencies:** All direct package imports are explicitly listed in `deps`.
* [ ] **Transitive Dependency Hygiene:** Transitive dependencies of embedded libraries (e.g. `:lib` or `:main`) are not duplicated in `go_binary_host_tool` `deps`.
* [ ] **Source Separation:** Sources are separated cleanly into `srcs`, `embedsrcs`, and `data`.
* [ ] **Test Source Separation:** Test sources (`*_test.go`) in GN `go_library` are separated into `host_go_test`.

---

## 3. Rust Review Criteria

### General Rust Target Rules
* [ ] **Rule Imports:** `rustc_binary` and `rustc_library` loaded from `//build/bazel/rules/rust:defs.bzl`.
* [ ] **Rust Edition:** Explicitly sets `edition = "2024"`.
* [ ] **Field Mappings:**
  - `output_name` in GN -> `crate_name` in Bazel.
  - `features` in GN -> `crate_features` in Bazel.
  - `lint_config` in GN -> `lint_config` in Bazel (or configs preserved where multiple lints configs are required).
* [ ] **Third-Party Dependencies:** Third-party crate references use the Bazel vendor path (e.g., `//third_party/rust_crates/vendor:anyhow` or `ask2patch`/`fork`/`intree`).
* [ ] **Crate Root:** If multiple `srcs` exist and the entry point is non-default (not `src/lib.rs` or `src/main.rs`), `crate_root` is explicitly set.
* [ ] **Target Shape Integrity:** Standalone GN `rustc_test` targets are not merged into `with_unit_tests`/`with_host_unit_tests`, and `rustc_library` with unit tests is not split into separate targets.

### Host-Specific Rust Rules
* [ ] **Host Unit Tests:** Unit tests for host targets use `with_host_unit_tests = True` on `rustc_binary` or `rustc_library` (use `with_unit_tests = True` for Fuchsia targets).
* [ ] **`ffx` Tools and Plugins:** Specialized Starlark macros `ffx_tool` and `ffx_plugin` (from `//src/developer/ffx/build/`) are used for subtools/plugins in `//src/developer/ffx/`.

---

## 4. GN Bridging with `bazel_host_tool()`

* [ ] **`bazel_host_tool()` Definition:** In `BUILD.gn`, a `bazel_host_tool()` target is defined using the same name as the migrated host binary:
  ```gn
  bazel_host_tool("my_tool") {
    bazel_target = ":my_tool"
    bazel_output_path = "{{BAZEL_TARGET_OUT_DIR}}/my_tool"
  }
  ```
* [ ] **Output Path Configuration (`bazel_output_path`):**
  - C/C++ or Rust: `"{{BAZEL_TARGET_OUT_DIR}}/<target_name>"`
  - Go: `"{{BAZEL_TARGET_OUT_DIR}}/<target_name>_/<target_name>"`
* [ ] **`install_host_tool` Field:** If the target was previously wrapped in `install_host_tools` in GN, set `install_host_tool = true` on `bazel_host_tool()` and remove the old `install_host_tools` wrapper.

---

## 5. GN Target References Update

* [ ] **GN References:** Dependent GN targets are updated from legacy target labels (e.g., `//tools/my_tool:my_tool`) to the `bazel_host_tool()` target or Bazel root host wrapper `//build/bazel/host:bazel_root_host_tools.{target_name}`.

---

## 6. Synchronizer (`bazel2gn`) & Verification Targets

* [ ] **Fully Migrated Directory (Delete `BUILD.gn`):**
  - If all targets in `{directory_path}/BUILD.gn` are migrated to Bazel and no external GN targets depend on them, `BUILD.gn` is deleted.
  - Do not run `bazel2gn`, and do not add `# @bazel2gn:skip`.
* [ ] **Partially Migrated Directory (Retain `BUILD.gn`):**
  - `# @bazel2gn:skip` is added on the line immediately preceding `go_binary_host_tool` or `rustc_binary` in `BUILD.bazel`.
  - Synced `BUILD.gn` does not retain unreferenced internal library targets.
* [ ] **Directive Clean-up:** `# @bazel2gn:skip` is removed (or omitted) if `BUILD.gn` is deleted.

---

## 7. Host Test Suite Integration & Test Parity

* [ ] **Go Host Tests:** Individual `host_go_test` targets are added directly to parent/ancestor `//tools:host_tests` (or `//build/tools:host_tests`) with `visibility = ["//tools:__pkg__"]` (no intermediate package `test_suite(name = "tests")`).
* [ ] **Rust Host Tests:** Migrated test targets are grouped under a package-level `"tests"` `test_suite()` target with visibility restricted to parent/ancestor package, and added to the parent `"host_tests"` `test_suite()`.
* [ ] **No GN Test Duplication:** Migrated host tests are removed from GN test groups (`group("tests_no_e2e")` in `//tools/BUILD.gn` or `group("tests")` in `//build/tools/BUILD.gn`), unless the library still has GN users: then the `bazel2gn`-generated GN copy of the test stays listed (tests of a library in both build systems run in both; see [Testing libraries in both build systems](../../../../../../../docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)).
* [ ] **Test Parity Check:** All tests removed from `BUILD.gn` have matching definitions in `BUILD.bazel` included in a Bazel test suite; unmigrated GN tests remain in GN.

---

## 8. Verification & Build Checks

* [ ] **Quick Tests:** `fx build //build/bazel/rules/tests:quick_tests` passes.
* [ ] **Direct Bazel Build:** `fx build --host @//{directory_path}:{target_name}` passes.
* [ ] **Bazel-in-GN Build:** `fx build --host //{directory_path}:{target_name}` passes.
* [ ] **Host Tests Execution:** `fx bazel test --config=host //{directory_path}:{test_target_name}` and aggregated `fx bazel test --config=host //tools:host_tests` pass.
* [ ] **bazel2gn Verification:** Synced directories pass the bazel2gn sync check (Verify step of the `syncing-bazel-to-gn` skill).
