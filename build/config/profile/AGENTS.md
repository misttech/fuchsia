# Coverage Build Configuration - AI Agent Guidelines (`//build/config/profile`)

When modifying GN or Bazel test coverage configurations, toolchains, or debug symbol registration, read [`README.md`](./README.md) first for the end-to-end architecture and cross-tree file index, and enforce the following invariants.

## 1. Architectural Invariants

* **Keep `README.md` in Sync**: Whenever you change compiler/linker flags, variant plumbing, `-fprofile-list` handling, or debug symbol registration for GN or Bazel coverage, update [`README.md`](./README.md) in the same CL so the architecture reference reflects HEAD.
* **Two-Channel Bazel Flag Plumbing (`ST-<hash>` Consistency)**:
  * Native and Starlark build flags (`--features=coverage`, `--//build/bazel/config/variables:is_profile=...`) that belong to a target or host platform should be routed via `_build_args` in [`//build/bazel/config/bazel_args.gni`](../../bazel/config/bazel_args.gni) into `@fuchsia_build_info//:platforms_flags.bzl` (`platform(flags = ...)`).
  * Route `.bazelrc` `--config=<name>` flags (such as `--config=host_rust_coverage` and `--config=fuchsia_rust_coverage` for `--@rules_rust//rust/settings:extra_rustc_flags=...`) via `_common_args` into `workspace/.bazelrc`; never place `--config=<name>` flags into `_build_args` (`platform(flags = ...)` rejects `--config`).
  * Any flag that alters Bazel's Starlark transition configuration checksum (`ST-<hash>` in `bazel-out/<cpu>-<mode>-ST-<hash>/bin/`) must be identically active during `bazel cquery` (`fx gen`) and `bazel build` (`fx build`), or manifest paths in `bazel_test_packages.list` / `bazel_host_tests.debug_symbols.json` will mismatch built outputs.
* **RBE & Sandbox Action Hermeticity**:
  * Never pass a file path in a compiler or linker flag (such as `-fprofile-list=<file>` or `-Clink-arg=<file>.ld`) without also declaring that file as an explicit action input in the corresponding toolchain (`compiler_files` in [`toolchain_utils.bzl`](../../bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl) for Clang, or `rust_stdlib_filegroup` in [`rust.BUILD.bazel`](../../bazel/toolchains/rust/rust.BUILD.bazel) for Rust).
* **Fail-Fast `.build-id` Completeness**:
  * [`ffx coverage`](../../../src/developer/ffx/plugins/coverage/src/lib.rs) fails fast if *any* binary ID present in *any* `.profraw` file is missing from `${FUCHSIA_BUILD_DIR}/.symbol-index.json` / `${FUCHSIA_BUILD_DIR}/.build-id/`. Ensure unstripped `.debug` binaries for both the main test executable and all in-tree instrumented shared libraries (`libfdio.so`, `libbackend_fuchsia_globals.so`, etc.) are materialized even under `--remote_download_minimal`.

## 2. Canonical Verification Targets & Commands

Verify coverage changes across both **C++** and **Rust** for local (`fx coverage`), full (`profile_source_files = ["//*"]`), and selective (`profile_source_files = ["//path/to/file"]`) configurations:

* **Bazel Target Package Tests (`fx_test()`)**:
  * **C++**: `//src/developer/build_info:build-info-unittest`, `//src/lib/files:files-tests`, `//src/lib/testing/predicates:predicates-lib-test`
  * **Rust**: `//src/lib/mapped-clock:mapped-clock-tests`, `//src/lib/detect-stall:detect-stall-unittests`, `//src/lib/async-utils:async-utils-tests`
* **Local Verification Commands**:
  ```bash
  # Enable both target and host coverage in args.gn:
  fx set core.x64 --variant coverage --variant host_coverage

  # Run local coverage on a Bazel target package test:
  fx coverage //src/lib/files:files-tests
  fx coverage //src/lib/mapped-clock:mapped-clock-tests
  ```
