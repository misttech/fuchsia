---
name: migrating-host-tool-to-bazel
description: >-
  Migrates Fuchsia host tools from GN to Bazel build systems. Use when
  converting go_binary, go_library, rustc_library, or rustc_binary targets from
  BUILD.gn to BUILD.bazel, syncing Bazel outputs back to GN using bazel2gn, or
  resolving migration dependency errors like missing importpaths or
  verify_bazel2gn targets.
---

# Host Tool Bazel Migration

This skill guides you through migrating host tools from GN to Bazel.

## Quick Checklist

Copy this checklist and track progress:

- [ ] Step 1: Confirm provided host targets build via GN
- [ ] Step 2: Validate dependencies are buildable with Bazel or migrate them first
- [ ] Step 3: Create BUILD.bazel with identical Bazel targets
- [ ] Step 4: Verify new Bazel target correctness natively
- [ ] Step 5: Update GN root tooling references
- [ ] Step 6: Sync to GN via `bazel2gn`, gracefully skipping binaries
- [ ] Step 7: Resolve any `bazel2gn` verification errors or broken `go_test`s
- [ ] Step 8: Format code
- [ ] Step 9: Execute full build validation

## Prerequisites

- Confirm the provided GN target is a host tool target with:

  ```bash
  fx build --host //{directory_path}:{target_name}
  ```

- Ensure that all dependencies of the host tool target are buildable from
  Bazel with the following command. If not, recursively migrate missing
  dependencies using this skill (migrating-host-tool-to-bazel):

  ```bash
  # Keep the @ prefix when building Bazel targets with `fx build`.
  fx build --host @//{dependency_path}:{dependency_name}
  ```

## Procedure

Follow these steps to migrate the targets from GN to Bazel:

### 1. Create Bazel targets

Create `BUILD.bazel` in the same directory as `BUILD.gn` if missing.
**NOTE:** Add the standard Fuchsia copyright header (`#`) to new `BUILD.bazel` files.

Refer to language-specific guides and examples:

- [Go Migration Guide](references/go_migration.md) (See **Common Pitfalls** for `importpath` and dependency gotchas).
- [Go Examples](examples/go)
- [Rust](references/rust_migration.md) (See **Common Pitfalls and Best Practices** section).

**NOTE:** Set `target_compatible_with = HOST_CONSTRAINTS` (or `HOST_OS_CONSTRAINTS` for tools in the IDK) on your Bazel targets. See [target_compatible_with.md](references/target_compatible_with.md).

### 2. Verify Bazel Target Correctness

Verify the new Bazel target builds correctly:

```bash
# Keep the @ prefix when building Bazel targets with `fx build`.
fx build --host @//{directory_path}:{target_name}
```

### 3. Update GN References

Update GN references to use the new Bazel host tool target following
instructions from
[bazel_root_targets_list.md](references/bazel_root_targets_list.md).

### 4. Sync to GN for Library and Test Targets

Follow the following steps for migrated library and test targets (e.g. `go_library`,
`rustc_library`, `rustc_test`, `source_set`, `static_library`):

**CRITICAL `bazel2gn` GOTCHAS:**

- **Prevent Redundant Binary Syncs:** Add `# @bazel2gn:skip` on the line
  immediately preceding `go_binary_host_tool` or `rustc_binary` in `BUILD.bazel` so it isn't
  output into GN as a binary.
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

2. Run `fx gen` to validate the GN build graph.
   - **NOTE:** If `fx gen` fails with missing GN targets, sync them back using
     [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md).
   - Only if you suspect the tests are completely missing from the active build configuration, reconfigure the build using:
     ```bash
     fx set core.x64 --with '//bundles/buildbot/core' --with '//bundles/tests'
     ```
     _(Avoid running `fx set` if possible, as it is slow and overwrites the active board/product configuration)._

### 5. Format Code

Format all changed files with:

```bash
fx format-code --parallel
```

### 6. Final Verification

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
