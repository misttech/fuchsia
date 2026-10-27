# `bazel2gn` AI Agent Guide (`//build/tools/bazel2gn`)

`bazel2gn` converts Starlark build targets in `BUILD.bazel` into GN targets
below `## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` in `BUILD.gn`.
Read [`README.md`](./README.md) first for user-facing concepts, design
motivation, and comment annotations.

## 1. Design Principles

-   **Keep `BUILD.bazel` idiomatic**: `BUILD.bazel` is the long-term source of
    truth; generated GN targets are temporary migration scaffolding.
-   **Adapt GN templates over inventing annotations**: When a Bazel rule and GN
    template have mismatched parameter conventions, prefer updating the `.gni`
    template in GN (preserving backward compatibility) rather than adding
    complex `bazel2gn` workarounds or comment annotations.
-   **Fail loudly on unsupported constructs**: Return descriptive errors for
    unhandled Starlark node types, unknown Bazel rules, or unsupported macro
    substitutions instead of silently dropping attributes.

## 2. Package Layout & Key Entry Points

-   [`bazel2gn.go`](./bazel2gn.go): Exported parser (`ParseBytes`) and top-level
    file/statement converters (`FileToGN`, `StmtToGN`), plus AST traversal for
    rule calls, variable assignments, `target_compatible_with`, `aspect_hints`
    reordering, same-package visibility, and `# @bazel2gn:raw_attribute:`
    annotations.
-   [`constants.go`](./constants.go): Rule-to-GN-template mappings
    (`bazelRuleToGNTemplate`, `noGNEquivalent`), attribute omissions
    (`attrsToOmitByRules`), attribute renames (`commonAttrMap`,
    `attrMapsByRules`), extra GN expressions (`extraGnExpressionByRules`),
    assignment operators (`bazelAttrToGNAssignmentOps`), platform constraints,
    `coptToConfig`, third-party repo mappings, and `@bazel2gn:*` annotation
    prefixes.
-   [`compiled_action.go`](./compiled_action.go): Converts `compiled_action`
    `outs` (placed under `$target_gen_dir` unless overwritten) and `args`
    (`$(location <label>)`, `$(execpath <label>)`, and `$(RULEDIR)`).
-   [`fx_component.go`](./fx_component.go): Validates `fx_component_manifest`
    and `fx_component`, skips `fx_component_manifest` targets consumed only by
    `fx_test_component` or a skipped `fx_component` in the same file, hoists
    remaining `fx_component_manifest` targets before earlier same-file
    consumers, and resolves `fx_packaged_binary` dependencies in `fx_component`
    `deps` (either unwrapping to the underlying binary target or emitting a GN
    `renamed_binary`).
-   [`fx_unittest_package.go`](./fx_unittest_package.go): Converts
    `fx_unittest_package` into `fuchsia_unittest_package` (or
    `fuchsia_unittest_component` + `fuchsia_test_package` for multiple test
    binaries) with a `-gn` suffix on `package_name`.
-   [`genrule.go`](./genrule.go): Converts `genrule` `cmd` strings starting with
    `$(location //path/to:tool)` into GN `script` and `args`.
-   [`rust_check_cfg.go`](./rust_check_cfg.go): Converts
    `rustc_check_cfg_build_flags` macro calls into GN `config` targets with
    `--check-cfg=cfg(...)` in `rustflags`.
-   [`rust_test_crate.go`](./rust_test_crate.go): Inlines `srcs`, `crate_root`,
    `edition`, `rustc_env`, `deps`, `proc_macro_deps`, `link_deps`, and
    `compile_data` for `rustc_test` / `host_rustc_test` targets that set
    `crate = ":<lib>"`.
-   [`rustenv.go`](./rustenv.go): Converts `rustc_env` dictionaries into GN
    `rustenv = ["KEY=VALUE", ...]` lists (supporting `#
    @bazel2gn:value_overwrite:`).
-   [`select.go`](./select.go): Converts `select()`, conditional expressions
    (`CondExpr`), and `+` list concatenations into GN `if / else if / else`
    statements.
-   [`tags.go`](./tags.go): Extracts `assert_no_deps=<label>` from Bazel `tags`
    into GN `assert_no_deps` (preserving comments for overwrites), and
    `exclude_toolchain_tags=<tag>` on Rust rules into GN
    `exclude_toolchain_tags`.
-   [`transformers.go`](./transformers.go): Attribute value transformers
    (`bazelVisibilityToGN`, `bazelCOptToGNConfig`, `bazelDepToGN`,
    `bazelFilePathsToGN`, `bazelLdflagsToGN`, `bazelExprToGNList`) and inline
    comment overwrite readers.
-   [`bazel_migration.gni`](./bazel_migration.gni): Umbrella `.gni` file
    imported below the sentinel in all generated `BUILD.gn` files.
-   [`third_party_target_map.json`](./third_party_target_map.json): Embedded
    JSON table mapping external Bazel repo labels (`@...`) to
    `//third_party/...` GN labels.
-   [`cmd/main.go`](./cmd/main.go): CLI host binary (`--gn_bin`,
    `--buildifier_bin`, `--fuchsia_dir`, `--directory_list`, `--check_only`,
    `--workers`) that discovers or reads target directories, formats
    `BUILD.bazel` with `buildifier`, preserves handwritten GN targets above the
    sentinel, runs `gn format`, and writes or verifies `BUILD.gn`.

## 3. Checklist: Adding or Updating a Rule Conversion

When adding support for a Bazel rule or attribute, check all five touchpoints:

1.  **Rule registration ([`constants.go`](./constants.go))**:
    -   Add the Bazel rule to `bazelRuleToGNTemplate`.
    -   Map Bazel-only rules (such as `fx_package`, `fx_test_component`,
        `fx_test`, or `host_cc_test`) to `noGNEquivalent` so `bazel2gn` skips
        them cleanly.
    -   If the rule requires custom target emission (like
        `rustc_check_cfg_build_flags`), intercept it in `callExprToGN` in
        [`bazel2gn.go`](./bazel2gn.go).
2.  **Attribute omissions, renames, and operators
    ([`constants.go`](./constants.go))**:
    -   Add Bazel-only attributes (e.g., `cgo`, `importpath`, `proto_deps`,
        `include_deps`, `size`) to `attrsToOmitByRules` with a comment
        explaining why GN omits them.
    -   Add attribute name mappings to `attrMapsByRules` (or `commonAttrMap` if
        shared across rules). Compose maps with `mustMergeMaps`; duplicate keys
        panic at startup.
    -   Add fixed GN assignments required by a rule (e.g., `sdk = "shared"`,
        `testonly = true`) to `extraGnExpressionByRules`.
    -   Add attributes requiring `+=` or `-=` in GN (such as `configs`,
        `build_flags`, `disable_build_flags`) to `bazelAttrToGNAssignmentOps`.
3.  **Value transformers ([`bazel2gn.go`](./bazel2gn.go) &
    [`transformers.go`](./transformers.go))**:
    -   In `attrAssignmentToGN` (`switch attrName`, where `attrName` is the
        converted GN name from `convertAttrName`), attach the right transformer
        when an attribute holds dependency labels (`bazelDepToGN`), file paths
        (`bazelFilePathsToGN`), visibility (`bazelVisibilityToGN`), or configs
        (`bazelCOptToGNConfig`).
    -   Place non-trivial rule or attribute converters in dedicated
        `<feature>.go` files (such as `compiled_action.go`, `genrule.go`,
        `rustenv.go`, `tags.go`) and register new `.go` and `*_test.go` files in
        [`BUILD.bazel`](./BUILD.bazel).
4.  **GN template imports ([`bazel_migration.gni`](./bazel_migration.gni))**:
    -   Import the `.gni` file defining any newly supported non-built-in GN
        template in `bazel_migration.gni`. Synced `BUILD.gn` files only import
        `//build/tools/bazel2gn/bazel_migration.gni`.
5.  **Unit tests**:
    -   Add table-driven conversion tests in
        [`bazel2gn_test.go`](./bazel2gn_test.go) or a dedicated
        `<feature>_test.go` file covering normal conversion, edge cases, and
        expected error paths.

## 4. Critical Invariants & Pitfalls

-   **`--check_only` must never write to disk**: `shac` runs `bazel2gn
    --check_only` against a read-only checkout mount during `fx lint` and CQ.
    `formatBazelFile` and `syncDirectory` in [`cmd/main.go`](./cmd/main.go) must
    format in memory via `stdin`/`stdout` and never modify `BUILD.bazel` or
    `BUILD.gn` when `--check_only` is set.
-   **`shac` output coupling (`LINT.IfChange`)**:
    [`//scripts/shac/bazel_migration.star`](../../../scripts/shac/bazel_migration.star)
    runs `bazel2gn --check_only --json` and depends on two things in
    `cmd/main.go`: `sentinelComment` (`LINT.IfChange(bazel2gn_output)`) and the
    fields of the JSON `diagnostic` struct (`LINT.IfChange(bazel2gn_json)`).
    Keep both sides of each block synchronized, and keep stdout valid JSON in
    `--json` mode (log to stderr).
-   **Avoid unnecessary `BUILD.gn` writes**: When `--check_only` is false and
    the formatted output matches the existing `BUILD.gn`, do not touch
    `BUILD.gn` on disk so `ninja` / `gn gen` timestamps remain clean.
-   **`cc_library` vs. `source_set`**: `cc_library` and `fx_cc_library` map to
    `static_library` by default. They only convert to `source_set` when
    `alwayslink = True` is set on the Bazel target.
-   **Same-package visibility (`":*"`)**: Bazel grants implicit same-package
    visibility, whereas GN does not. `ensureSamePackageVisibility` injects
    `":*"` into restricted GN visibility lists unless `"*"`, `"//*"`, `":*"`, or
    `"./*"` is already present.
-   **`aspect_hints` ordering**: `buildifier` sorts `aspect_hints` before `deps`
    / `implementation_deps`, but all three can map to `deps` in GN.
    `reorderAspectHints` moves `aspect_hints` after other `deps`-mapped
    attributes and uses `+=` (emitting `deps = []` first if earlier `deps`
    assignments are conditional).
-   **`exec_properties` is Bazel-only**: GN templates don't accept it, so
    `execPropertiesToGN` maps `exec_properties = RBE_USE_LARGE_WORKER` on Rust
    rules to `concurrent_jobs.rust_highmem` (via a `_concurrent_jobs`
    variable, because GN rejects nested scope access like `a.b.c`) and fails on
    anything else. Never emit `exec_properties` verbatim.
-   **Top-level variable assignments**: `assignStmtToGN` emits `not_needed([
    "<var>" ])` immediately after each top-level assignment so GN does not fail
    with `"Assignment had no effect"` when variables are only referenced inside
    conditional blocks.
-   **`select()` conditions**: `//conditions:default` cannot be the first or
    sole branch; `//build/bazel/platforms:is_fuchsia_with_sdk_rules` always maps
    to `false` in GN because the GN build is always a platform build.
-   **Comment annotation constraints**: `# @bazel2gn:raw_attribute:` must be
    non-empty, placed immediately above a converted rule target (not separated
    by blank lines or unrelated comments), and is rejected on attributes,
    variable assignments, `rustc_check_cfg_build_flags`, `package`, and
    `noGNEquivalent` rules. Overwriting `ldflags` requires `#
    @bazel2gn:raw_overwrite:` (`# @bazel2gn:path_overwrite:` is rejected).
-   **Third-party label mapping
    ([`third_party_target_map.json`](./third_party_target_map.json))**: External
    repo labels (`@...`) are resolved via `third_party_target_map.json`
    (embedded into `bazel2gn_lib` via `//go:embed`). Keep keys sorted and remove
    mappings when external repositories are retired.
-   **Self-hosting build files**: [`BUILD.bazel`](./BUILD.bazel) is the source
    of truth for building `bazel2gn`. [`BUILD.gn`](./BUILD.gn) wraps `:bazel2gn`
    via `bazel_host_tool("bazel2gn")` and intentionally does **not** use the
    `bazel2gn` sentinel.

## 5. Documentation Sync

-   Update [`README.md`](./README.md) in the same change whenever you add or
    change comment annotations, rule-specific conversion constraints (like
    `compiled_action` substitutions), or third-party label handling.

## 6. Verification

Run unit tests, re-sync tree-wide `BUILD.gn` files, and run the `shac` check
before finishing a change:

```bash
# Run library and CLI unit tests:
fx bazel test --config=host //build/tools/bazel2gn:bazel2gn_tests //build/tools/bazel2gn:bazel2gn_cmd_tests

# Re-sync all BUILD.gn files containing the bazel2gn sentinel:
fx bazel2gn

# Or sync a single directory during iteration:
fx bazel2gn -d path/to/dir

# Rebuild the host tool in the Ninja build dir and run the shac sync check:
fx build --host //build/tools/bazel2gn:bazel2gn
fx lint --only=verify_bazel2gn
```
