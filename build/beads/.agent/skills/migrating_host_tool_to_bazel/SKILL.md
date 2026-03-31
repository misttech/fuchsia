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

This skill guides you through migrating host tools from GN to Bazel. The host
tool is provided to you as a GN target in `BUILD.gn` files.

## Persona

You are a Fuchsia build system expert with deep knowledge of both GN and Bazel.
You are familiar with the Fuchsia build graph structure and the process of
migrating build targets between GN and Bazel.

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

Create a `BUILD.bazel` file in the same directory as `BUILD.gn` if it doesn't
exist. Define the equivalent Bazel targets.
**NOTE:** Always add the standard Fuchsia copyright/license header formatted
for Python (`#`) to all newly created `BUILD.bazel` files.

Based on the source language the GN target is written in, reference the
corresponding language-specific migration guide for more information:

- [Go](references/go_migration.md) (See **Common Pitfalls** section for
  important `importpath` and dependency tracking instructions).

And the language-specific migration examples:

- [Go examples](examples/go)

**NOTE:** Set `target_compatible_with = HOST_CONSTRAINTS` (or
`HOST_OS_CONSTRAINTS` for host tools included in the IDK) for the Bazel
targets you create. See
[target_compatible_with.md](references/target_compatible_with.md) for more
information.

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

### 4. Sync to GN For Library Targets

Follow the following steps for migrated library targets (e.g. `go_library`,
`rustc_library`, `source_set`, `static_library`):

**CRITICAL `bazel2gn` GOTCHAS:**

- **Prevent Redundant Binary Syncs:** Add `# @bazel2gn:skip` on the line
  immediately preceding `go_binary_host_tool` in `BUILD.bazel` so it isn't
  output into GN as a `go_binary`.
- **Missing `verify` Targets:** Every synchronized directory outputs a
  `verify_bazel2gn` target. You MUST manually add
  `//{directory_path}:verify_bazel2gn` to `group("bazel2gn_verifications")`
  in `//build/BUILD.gn` to hook it into the main build graph.

1. Remove the targets you've migrated from `{directory_path}/BUILD.gn`.

2. Run the following command. If it fails with missing GN targets, sync
   migrated targets back to GN using skill
   [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md):

```bash
fx set core.x64 --with '//bundles/buildbot/core' --with '//bundles/tests'
```

### 5. Format Code

Format all changed files with:

```bash
fx format-code --parallel
```

### 6. Final Verification

Run the following command and ensure it returns successfully, fix any build
errors that arise:

```bash
fx set core.x64 --with '//bundles/buildbot/core' --with '//bundles/tests'
fx build --host //build/bazel/host:bazel_root_host_tools
```
