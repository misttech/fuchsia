# Fuchsia Platform C/C++ Bazel Rules and Macros

This directory contains Fuchsia-specific C/C++ wrapper macros, rules, and
providers for platform builds.

See [Use Fuchsia-specific wrappers](/docs/development/languages/bazel/style/common.md#use-fuchsia-specific-wrappers)
for general usage guidance. For Zircon libraries (`zx_library()` in GN), see
[`//build/bazel/rules:zx_library.bzl`](/build/bazel/rules/zx_library.bzl)
(`cc_shared_library_zx()`, `cc_source_library_zx()`, `cc_static_library_zx()`).

## Contents

* [`fx_cc_binary.bzl`](./fx_cc_binary.bzl): Defines `fx_cc_binary()`, Fuchsia's
  wrapper macro for `@rules_cc`'s `cc_binary()`. Also used in place of
  `cc_test()`.
* [`fx_cc_library.bzl`](./fx_cc_library.bzl): Defines `fx_cc_library()`,
  Fuchsia's wrapper macro for `@rules_cc`'s `cc_library()`.
* [`fx_cc_library_headers.bzl`](./fx_cc_library_headers.bzl): Defines
  `fx_cc_library_headers()`, a macro for defining a set of public headers for a
  C++ library.
* [`providers.bzl`](./providers.bzl): Defines C++ library providers (such as
  `PrebuiltLibraryInfo`).
* [`shared_library.bzl`](./shared_library.bzl): Defines rules and macros for
  shared library ABI verification (`verify_public_symbols()`) and `.ifs` link
  stub generation (`link_stub_from_ifs_file()`, `cc_link_stub_from_ifs_file()`).

## Adding a New Wrapper Macro or Rule

When adding a new Fuchsia wrapper macro or rule equivalent for an upstream
`@rules_cc` symbol (for example, `fx_cc_shared_library()` for
`cc_shared_library()`):

1. **Follow Fuchsia naming and style guidelines**:
   * See [Wrapping built-in and common rules, macros, and functions](/docs/development/languages/bazel/style/rules_macros.md#wrapping-built-in-and-common-rules-macros-and-functions).
   * Prefix the wrapper name with `fx_` (never `fuchsia_`) and define it in
     `//build/bazel/rules/cc/<wrapper_name>.bzl`.
2. **Register the wrapper in the `bazel_fuchsia_wrappers` SHAC check**:
   * Add an entry to `_WRAPPED_RULES` in
     [`//scripts/shac/bazel_lint/checks/fuchsia_wrappers.py`](/scripts/shac/bazel_lint/checks/fuchsia_wrappers.py)
     mapping the upstream `@rules_cc//` symbol to the new wrapper symbol and
     module label.
3. **Add unit tests for the SHAC check**:
   * Update [`//scripts/shac/bazel_lint/checks/tests/fuchsia_wrappers_test.star`](/scripts/shac/bazel_lint/checks/tests/fuchsia_wrappers_test.star)
     to verify that loading or calling the upstream symbol (including aliased
     loads and `native.*` calls) is flagged as an error, and that loading and
     calling the new wrapper is allowed.
4. **Migrate or allowlist pre-existing uses**:
   * Run `fx host-tool shac check --only bazel_fuchsia_wrappers --all` across
     the repository.
   * Migrate existing in-tree uses to the new wrapper where feasible, or add
     remaining pre-existing violations to
     [`//scripts/shac/bazel_lint/allowlists/bazel_fuchsia_wrappers.json`](/scripts/shac/bazel_lint/allowlists/bazel_fuchsia_wrappers.json)
     under `https://fxbug.dev/570203009`, following the sorting and formatting
     rules in
     [`//scripts/shac/bazel_lint/allowlists/README.md`](/scripts/shac/bazel_lint/allowlists/README.md).
5. **Update documentation**:
   * Update [`//docs/development/languages/bazel/style/common.md`](/docs/development/languages/bazel/style/common.md#use-fuchsia-specific-wrappers)
     if the new wrapper introduces a new naming pattern or module path.
   * Update [`//docs/development/languages/bazel/style/rules_macros.md`](/docs/development/languages/bazel/style/rules_macros.md#wrapping-built-in-and-common-rules-macros-and-functions)
     if the new wrapper introduces a new naming convention or exception.
   * Document the new `.bzl` file and symbol in the [Contents](#contents)
     section above.
6. **Verify tests and checks**:
   * Run `fx test --host @//scripts/shac/tests:bazel_test`.
   * Run `fx host-tool shac check --only bazel_fuchsia_wrappers --all`.
   * Run `fx doc-checker --local-links-only`.
