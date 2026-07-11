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

2. Refer to the template in `assets/copyright_header_template.md` to add the
   copyright header to the top of the `BUILD.bazel` file.

3. Refer to language-specific guides and examples to create bazel targets in
   the BUILD.bazel file.

- [Go Migration Guide](references/go_migration.md)
  (See **Common Pitfalls** for `importpath` and dependency gotchas).
- [Go Examples](examples/go)
- [Rust](references/rust_migration.md)
  (See **Common Pitfalls and Best Practices** section).

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

### Step 4: Sync to GN for Library and Test Targets

Follow the following steps for migrated library and test targets
(e.g. `go_library`, `rustc_library`, `rustc_test`, `source_set`, `static_library`):

**CRITICAL `bazel2gn` GOTCHAS:**

- **Prevent Redundant Binary Syncs:** Add `# @bazel2gn:skip` on the line
  immediately preceding `go_binary_host_tool` or `rustc_binary` in `BUILD.bazel`
  so it isn't output into GN as a binary.
- **Missing `verify` Targets:** Every synchronized directory outputs a
  `verify_bazel2gn` target. You MUST manually add
  `"//{directory_path}:verify_bazel2gn"` to the `bazel2gn_verification_targets`
  list in `//build/bazel2gn_verification_targets.gni` (or
  `//sdk/fidl/bazel2gn_verification_targets.gni` for FIDL targets) to hook it
  into the main build graph.
- **Testing Host Tests:** If you need to add the migrated host tests to the active
  build configuration for verification:
  - **Pitfall:** Running `fx add-test` on host-only tests will fail with
    unresolved target toolchain (e.g., `fuchsia:arm64`) dependencies.
  - **Fix:** Always use `fx add-host-test` instead of `fx add-test` for host
    tests.

1. Remove the targets you've migrated from `{directory_path}/BUILD.gn`.

2. Sync the target back from Bazel to GN using the `syncing-bazel-to-gn` skill
   (see `../syncing_bazel_to_gn/SKILL.md`).

3. Run `fx gen` to validate the GN build graph.
   - **NOTE:** If `fx gen` fails with missing GN targets, sync them back using
     [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md).
   - Only if you suspect the tests are completely missing from the active build
     configuration, reconfigure the build using:
     ```bash
     fx set core.x64 --with '//bundles/buildbot/core' --with '//bundles/tests'
     ```
     (Avoid running `fx set` if possible, as it is slow and overwrites the active board/product configuration).

### Step 5: Remove Redundant GN Targets
1. In the synced BUILD.gn file:
- if the `go_library` targets is not referenced by other targets, remove it.
- if there is no targets in the synced BUILD.gn, remove the BUILD.gn file.
2. In the BUILD.bazel file, if the BUILD.gn is removed:
  - Remove the `# @bazel2gn:skip` added on the line immediately preceding
    `go_binary_host_tool` or `rustc_binary`.
  - Remove the entry `"//{directory_path}:verify_bazel2gn"` added to the
    `bazel2gn_verification_targets` list.

### Step 6: Format Code

Format all changed files with:

```bash
fx format-code --parallel
```

### Step 7: Final Verification

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
