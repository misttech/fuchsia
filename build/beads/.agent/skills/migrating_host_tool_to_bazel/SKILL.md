---
name: migrating-host-tool-to-bazel
description: Migrate host tools from GN to Bazel
---

# Host Tool Bazel Migration

This skill guides you through migrating host tools from GN to Bazel. The host
tool is provided to you as a GN target in `BUILD.gn` files.

## Persona

You are a Fuchsia build system expert with deep knowledge of both GN and Bazel.
You are familiar with the Fuchsia build graph structure and the process of
migrating build targets between GN and Bazel.

## Prerequisites

- Confirm the provided GN target is a host tool target with:

  ```bash
  fx build --host //path/to/dir:tool
  ```

- Ensure that all dependencies of the host tool target are buildable from
  Bazel with the following command. If not, migrate the missing dependencies
  first.

  ```bash
  # Keep the @ prefix when building Bazel targets with `fx build`.
  fx build --host @//path/to/your:dependency_in_bazel
  ```

## Procedure

Follow the following steps closely to migrate the targets from GN to Bazel:

### 1. Create Bazel targets

Create a `BUILD.bazel` file in the same directory as `BUILD.gn` if it doesn't
exist. Define the equivalent Bazel targets.

Based on the source language the GN target is written in, reference the
corresponding language-specific migration guide for more information:

- [Go](references/go_migration.md)

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
fx build --host @//path/to/dir:tool
```

### 3. Update GN References

Update GN references to use the new Bazel host tool target following
instructions from
[bazel_root_targets_list.md](references/bazel_root_targets_list.md).

### 4. Sync to GN For Library Targets

Follow the following steps for migrated library targets (e.g. `go_library`,
`rustc_library`, `source_set`, `static_library`):

1. Remove the targets you've migrated from `path/to/dir/BUILD.gn`.

2. Run the following command, if it fails with missing GN targets, sync migrated
   targets back to GN using skill [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md):

```bash
fx set core.x64 --with '//bundles/buildbot/core' --with '//bundles/tests'
```

### 5. Final Verification

Run the following command and ensure it returns successfully, fix any build
errors that arise:

```bash
fx set fuchsia.x64 --with '//bundles/buildbot/core' --with '//bundles/tests' && fx build
```
