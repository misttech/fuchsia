---
name: migrating-host-tool-to-bazel
description: >-
  Migrates Fuchsia host tools from GN to Bazel build systems. Use when
  converting go_binary, go_library, rustc_library, or rustc_binary targets from
  BUILD.gn to BUILD.bazel, syncing Bazel outputs back to GN using bazel2gn, or
  resolving migration dependency errors like missing importpaths or
  verify_bazel2gn targets.
---

## Checks Before Migration

1. Confirm the provided GN target is a host tool target with:

  ```bash
  fx build --host //{directory_path}:{target_name}
  ```

2. Get the dependency tree of the host tool targets with following command line.

  ```bash
  # <target_label> e.g. "//path/to/directory:target_name"
  fx gn desc $(fx get-build-dir) "<target_label>(//build/toolchain:host_x64)" deps --tree
  ```

  Ensure that all dependencies of the host tool target are buildable from
  Bazel with the following command. If not, recursively migrate missing
  dependencies using this skill (migrating-host-tool-to-bazel):

  ```bash
  # Keep the @ prefix when building Bazel targets with `fx build`.
  fx build --host @//{dependency_path}:{dependency_name}
  ```

3. If the BUILD.bazel file already exists, check if the actual binary targets
   have been migrated. If so, warn user and stop the migration process.


## Migration Steps

### Step 1: Create Bazel targets
1. Create a `BUILD.bazel` file in the same directory as the `BUILD.gn` of the
   migrated target.

2. Refer to the template in `assets/build_bazel_header_template.md` to add the
   copyright header and `package(default_applicable_licenses = ["//:license"])`
   declaration to the top of the `BUILD.bazel` file.
   - **NOTE:** Place `package(...)` after any `load(...)` statements, as
     required by Bazel syntax.

3. Refer to language-specific guides and examples to create bazel targets in
   the BUILD.bazel file.
   - **NOTE:** Avoid setting package-level default visibility (`package(default_visibility = [...])`).
     Instead, set `visibility` explicitly on individual targets to be as
     restrictive as possible, preventing unintended dependencies.

- [Go Migration Guide](references/go_migration.md)
  (See **Common Pitfalls** for `importpath` and dependency gotchas).
- [Rust](references/rust_migration.md)
  (See **Common Pitfalls and Best Practices** section).

4. For host tests (e.g., `go_test`, `rustc_test`, `cc_test`):
   - Identify the `"host_tests"` `test_suite()` target in the parent or other ancestor
     directory (e.g., `//tools:host_tests` in [`//tools/BUILD.bazel`](//tools/BUILD.bazel)
     or `//build/tools:host_tests` in [`//build/tools/BUILD.bazel`](//build/tools/BUILD.bazel))
     of the migrated host tool.
      - If no ancestor `"host_tests"` `test_suite()` target exists, ask the user for guidance.
   - Group migrated host tool test targets in a package-level `"tests"` `test_suite()` target.
     Set its `visibility` to the parent/ancestor package containing the `"host_tests"`
     `test_suite()` identified above. For example, `visibility = ["//tools:__pkg__"]`
     or `visibility = ["//build/tools:__pkg__"]`.
   - Register the new package-level `"tests"` target in the `"host_tests"` `test_suite()`.
   - **Remove migrated tests from GN:** Remove the migrated test from the parent GN test
     group (e.g., `group("tests_no_e2e")` in `//tools/BUILD.gn` or `group("tests")` in
     `//build/tools/BUILD.gn`) so GN does not depend on deleted GN test targets. Ensure
     any un-migrated tests remain in GN.

**NOTE:** Set `target_compatible_with = HOST_CONSTRAINTS` (or `HOST_OS_CONSTRAINTS`
for tools in the IDK) on your Bazel targets.
See [target_compatible_with.md](references/target_compatible_with.md).

### Step 2: Verify Bazel Target Correctness

Verify the new Bazel target builds correctly:

```bash
# Keep the @ prefix when building Bazel targets with `fx build`.
fx build --host @//{directory_path}:{target_name}
```

### Step 3: Update GN References
Find external targets which references or depend on the original GN host tool
target with following command line.

```bash
# <target_label> e.g. "//path/to/directory:target_name"
fx gn refs $(fx get-build-dir) "<target_label>"
```

Update the references in the external targets to use the new Bazel host tool
targets following instructions from
[bazel_root_targets_list.md](references/bazel_root_targets_list.md).

### Step 4: Handle GN Targets and BUILD.gn

Determine whether `{directory_path}/BUILD.gn` can be deleted directly or requires `bazel2gn` syncing:

- **Case 1: All targets in `{directory_path}/BUILD.gn` are migrated:**
  If all targets in `BUILD.gn` have been migrated to Bazel and no other GN targets depend on targets in this directory:
  1. Simply **delete `{directory_path}/BUILD.gn`**.
  2. **Do NOT sync back from Bazel to GN:** Do NOT run `bazel2gn`, do NOT add `# @bazel2gn:skip` in `BUILD.bazel`, and do NOT add `verify_bazel2gn` to `//build/bazel2gn_verification_targets.gni`.
  3. Run `fx gen` to validate the GN build graph.

- **Case 2: Unmigrated GN targets remain or external GN targets still depend on libraries in `{directory_path}`:**
  1. Remove the migrated targets from `{directory_path}/BUILD.gn`.
  2. **Prevent redundant binary syncs:** Add `# @bazel2gn:skip` on the line immediately preceding `go_binary_host_tool` or `rustc_binary` in `BUILD.bazel` so it isn't output into GN as a binary.
  3. **Sync back to GN:** Sync the required library targets back from Bazel to GN using the `syncing-bazel-to-gn` skill (see `../syncing_bazel_to_gn/SKILL.md`).
  4. **Add verification target:** Add `"//{directory_path}:verify_bazel2gn"` to the `bazel2gn_verification_targets` list in `//build/bazel2gn_verification_targets.gni` (or `//sdk/fidl/bazel2gn_verification_targets.gni` for FIDL targets) to hook it into the main build graph.
  5. Run `fx gen` to validate the GN build graph.
     - **NOTE:** If `fx gen` fails with missing GN targets, sync them back using [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md).
  6. **Clean up redundant GN targets:**
     - In the synced `BUILD.gn` file, if a library target (e.g., `go_library`) is not referenced by other GN targets, remove it.
     - If there are no targets left in the synced `BUILD.gn`, remove `BUILD.gn`, remove `# @bazel2gn:skip` from `BUILD.bazel`, and remove `"//{directory_path}:verify_bazel2gn"` from `bazel2gn_verification_targets.gni`.

**Testing Host Tests:** If you need to add migrated host tests to the active build configuration for verification:
- **Pitfall:** Running `fx add-test` on host-only tests will fail with unresolved target toolchain (e.g., `fuchsia:arm64`) dependencies.
- **Fix:** Always use `fx add-host-test` instead of `fx add-test` for host tests.

### Step 5: Format Code

Format all changed files with:

```bash
fx format-code --parallel
```

### Step 6: Final Verification

Ensure everything builds correctly using the new Bazel targets:

```bash
# Regenerate build files
fx gen

# Build the Bazel-in-GN host tool target directly
fx build --host //build/bazel/host:bazel_root_host_tools.{target_name}

# Build all Bazel host tools to check for regressions
fx build --host //build/bazel/host:bazel_root_host_tools
```

If you want to verify host tests, add and run them:

```bash
fx add-host-test //{directory_path}:{test_target_name}
fx test //{directory_path}:{test_target_name}
```
