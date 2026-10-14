---
name: determining-bazel-visibility
description: >
  Determines the appropriate Bazel visibility for targets being migrated from
  GN to Bazel. Use when migrating a target or authoring BUILD.bazel and
  needing to infer visibility based on GN reverse dependencies (fx gn refs),
  code search, and intelligent package grouping rules.
---

# Determining Bazel Visibility for Migrated Targets

## Overview

When migrating build targets from GN to Bazel in Fuchsia, properly scoping
target visibility is critical:

- **GN Visibility Convention:** GN targets default to **public** (`"*"`)
  visibility if no `visibility` list is specified. Consequently, most legacy GN
  targets do not declare explicit visibility.
- **Bazel Visibility Convention:** Bazel targets default to **private**
  (`["//visibility:private"]`), visible only within the same package
  (`BUILD.bazel`).

Visibility states who should be allowed to use a target. It is not a golden file
of who uses it today. Use the current consumers found below as evidence of the
target's audience, then widen the list to that audience (for example, a whole
subsystem) so the next legitimate user doesn't have to edit it. More than about
five entries usually means the list is tracking consumers rather than describing
an audience; group them or use a `package_group`.

The exception is deprecated targets. For those, list exactly the current users,
however many there are, so the visibility list blocks new usages.

> [!TIP]
> **Subagent Delegation:** Determining visibility involves running queries across the
> dependency graph, discovering consumers across the codebase, and applying grouping
> heuristics. Because this adds significant context to the conversation, it is
> recommended to delegate the execution of this skill to a subagent (via `invoke_subagent`)
> to keep the main agent context concise.

### Anti-Patterns to Avoid

- **Do NOT set package-level default visibility:** Avoid
  `package(default_visibility = [...])`. Always set `visibility` directly on the
  specific target.
- **Do NOT use `"//visibility:public"`:** Do NOT use `"//visibility:public"`
  outside macros and rules exposed in the Bazel SDK (all are in
  `//build/bazel_sdk/...`) or a small number of targets exposed from other Bazel
  external repositories.
  - **NOTE:** Never use `"//visibility:public"` when migrating GN targets to
    Bazel.
- **Do NOT default to `["//:__subpackages__"]`:** Blindly making migrated
  targets public defeats Bazel encapsulation, exposes internal platform details,
  and clutters the build graph.
- **Do NOT use `"//:__subpackages__"` to allow access to targets defined in the
  root package (`//`):** Always use `"//:__pkg__"` for such cases.
- **Do NOT list dozens of individual `:__pkg__` directories:** If multiple
  subdirectories (for example, 5 or more packages) within the same subsystem
  depend on a target, do not enumerate each individual package path. Use
  intelligent package grouping (`__subpackages__`).
- **Do NOT use overly broad ancestor wildcards:** Avoid `//:__subpackages__` or
  `//src:__subpackages__` as a lazy shortcut. Such broad wildcards should only
  be considered for truly common libraries with widespread platform usage where
  specific package paths or package groups cannot be applied (see Case G).

---

## Step-by-Step Workflow

### Step 1: Check Existing GN Visibility

Before querying consumers, inspect the legacy GN target in `BUILD.gn` to see if
it already specifies an explicit `visibility` declaration:

1.  **Target has an explicit, restricted `visibility` list (other than `"*"`):**
    Translate GN patterns directly into Bazel syntax:
   - `"//path/to/dir/*"` in GN -> `"//path/to/dir:__subpackages__"` in Bazel.
   - `"//path/to/dir:*"` in GN -> `"//path/to/dir:__pkg__"` in Bazel.
   - `"//path/to/dir:some_target"` in GN -> `"//path/to/dir:__pkg__"` in Bazel.
   - `"//path/to/dir:some_group"` in GN -> `"//path/to/dir:some_group"` in Bazel
     (if referring to a `package_group`). *Skip Step 2 and proceed directly to
     **Step 3** to review and verify.*

2.  **Target has NO `visibility` declaration, or declares `visibility = [ "*"
    ]`:** In GN, omitting visibility defaults to public (`"*"`). Treat this as
    having no visibility constraint and proceed to **Step 2** to discover actual
    reverse dependencies.

---

### Step 2: Discover Reverse Dependencies (Dependers)

If the GN target did not specify restricted visibility, identify all existing
targets in the codebase that depend on it. These consumers show who the target's
audience is. Step 3 turns them into a description of that audience instead of
copying the list.

#### 1. Primary Tool: `fx gn refs`

Query GN's configured dependency graph for direct reverse dependencies:

```bash
# Query dependers in the default toolchain
fx gn refs $(fx get-build-dir) "<gn_target_label>"

# Example:
fx gn refs $(fx get-build-dir) "//src/developer/debug/zxdb:core"

# For host tools or host libraries, check the host toolchain as well:
fx gn refs $(fx get-build-dir) "<gn_target_label>(//build/toolchain:host_x64)"

# To see the exact BUILD.gn files containing the references:
fx gn refs $(fx get-build-dir) "<gn_target_label>" --as=buildfile
```

#### 2. Secondary Tool: Code Search

`fx gn refs` only inspects targets reachable in your current `fx set` /
`args.gn`. Complement it with code search to discover consumers across inactive
product configurations, vendor overlays, or already-migrated `BUILD.bazel`
files:

- **Internal Scoping:** If available in your environment, use internal code
  search scoped across internal integration and vendor repositories
  (`//vendor/...`) to ensure proprietary consumers are not missed. Otherwise,
  fall back to public code search (with the understanding that internal
  integration visibility may need to be expanded later).
- Search for the full target label or directory in build files:
  - Query: `f:BUILD\.(gn|bazel)$ "<target_directory>:<target_name>"`
  - Query: `f:BUILD\.(gn|bazel)$ "<target_directory>"` (Note: When
    `<target_name>` matches `<target_directory>`, GN style allows omitting the
    label name, e.g. `//src/foo` instead of `//src/foo:foo`).
- Intra-package references (e.g., `:my_target`) do not need to be searched for
  external visibility purposes, because Bazel targets are always visible to
  other targets within the same package.

#### 3. Normalize Consumer Packages

1.  Strip target names and toolchains to get the consumer package paths (e.g.,
    `//src/developer/debug/zxdb/console:foo(//build/toolchain:host_x64)` becomes
    `//src/developer/debug/zxdb/console:__pkg__`).
2.  **Exclude Self-Package:** Remove the target's own package path. Targets in
    the same `BUILD.bazel` file already have access under Bazel's default
    private visibility.
3.  Deduplicate the resulting set of external consumer package paths.

---

### Step 3: Apply Intelligent Package Grouping ("This Group is Fine")

Instead of listing every individual consumer directory, distill the consumer set
into concise, idiomatic Bazel visibility expressions using these heuristics:

#### Case A: No External Consumers (Intra-Package Only)
- **Condition:** All dependers are within the same directory, or only internal
  binary/test targets in the same package use it.
- **Rule:** Omit the `visibility` attribute entirely (defaulting to
  package-private), or set `visibility = ["//visibility:private"]`.
- **Example:**
  ```starlark
  # Private by default; no visibility needed
  rustc_library(
      name = "internal_helper",
      srcs = [ ... ],
  )
  ```

#### Case B: Target and Subpackages / Tests
- **Condition:** The target is consumed only by its own package and child
  subpackages (e.g., `tests/`, `test/`, `testing/`, `bin/`, `cmd/`).
- **Rule:** Use `[":__subpackages__"]` (or
  `["//<current_package>:__subpackages__"]`).
- **Example:**
  ```starlark
  visibility = [":__subpackages__"]
  ```

#### Case C: Same Subsystem / Cohesive Module (Lowest Common Ancestor)
- **Condition:** All external consumers reside within the same functional
  subsystem or module tree (e.g., all under `//src/developer/debug/...`,
  `//src/storage/fxfs/...`, or `//tools/check-licenses/...`).
- **Rule:** Find the Lowest Common Ancestor (LCA) directory of all consumers:
  - If the LCA is a meaningful subsystem boundary (directory depth >= 2, e.g.
    `//src/developer/debug` or `//tools/ffx`), use
    `["//<lca>:__subpackages__"]`.
  - **Guardrail:** Do **NOT** collapse all the way up to
    `//src:__subpackages__`, `//:__subpackages__`, or
    `//vendor:__subpackages__`. If consumers cross major top-level boundaries,
    use Cluster Grouping (Case F).
- **Example:** Target in `//src/storage/lib/vfs/cpp` consumed across
  `//src/storage/fxfs`, `//src/storage/minfs`, `//src/storage/fshost`:
  ```starlark
  visibility = ["//src/storage:__subpackages__"]
  ```

#### Case D: Assembly Input Bundles (AIBs)
- **Condition:** The target is packaged or referenced by Fuchsia Assembly
  (`//bundles/assembly/...` or `//bundles/assembly/bazel_inputs/...`).
- **Rule:** Grant visibility to `//bundles/assembly:__subpackages__`:
  ```starlark
  visibility = ["//bundles/assembly:__subpackages__"]
  ```

#### Case E: Test Targets and Test Suites
- **Condition:** Migrated test targets (both host tests and device tests)
  integrated into a parent package test suite (e.g., `//tools:host_tests` or a
  subsystem test suite).
- **Rule:** Set `visibility = ["//<parent_package>:__pkg__"]` (e.g.,
  `["//tools:__pkg__"]` or `["//build/tools:__pkg__"]`).

#### Case F: Multi-Subsystem Cluster Grouping
- **Condition:** Consumers span 2 or 3 distinct subsystems (e.g. consumers in
  `//src/diagnostics/...` and `//src/sys/...`, or related driver libraries
  across `//sdk/lib/driver_component/...`, `//sdk/lib/driver/...`, and
  `//src/devices/...`).
- **Rule:** Group by the respective subsystem roots using `__subpackages__`
  rather than listing separate package paths:
  ```starlark
  visibility = [
      "//src/diagnostics:__subpackages__",
      "//src/sys:__subpackages__",
  ]
  ```
- **Shared Variables & Package Groups:**
  - If multiple targets in the same `BUILD.bazel` file share the same visibility
    list, define a local Starlark list variable at the top of the file to keep
    the definitions readable and DRY:
    ```starlark
    _SHARED_DRIVER_VISIBILITY = [
        "//sdk/lib/driver:__subpackages__",
        "//sdk/lib/driver_component:__subpackages__",
        "//src/devices:__subpackages__",
    ]
    ```
  - If several packages would repeat the same long visibility list, define a
    `package_group` once and reference it from each target instead. See [Use
    package groups for common non-trivial visibility
    definitions](../../../../../docs/development/languages/bazel/style/common.md#use-package-groups-for-common-non-trivial-visibility-definitions).

#### Case G: Widely Consumed Platform Libraries / Disparate Top-Level Trees
- **Condition:** Consumers span 3 or 4 disparate top-level trees (e.g., `//src`,
  `//tools`, `//vendor`, `//examples`), or the target is an IDK library used
  across multiple subsystems.
- **Rule:**
  - Do **NOT** use `"//visibility:public"`.
  - Instead, specify the respective top-level subsystem roots (or predefined
    package groups if available) to protect boundary encapsulation (e.g.,
    keeping internal platform libraries from being directly depended on by
    `//boards`, `//bundles`, or external consumers):
    ```starlark
    visibility = [
        "//src:__subpackages__",
        "//tools:__subpackages__",
        "//vendor:__subpackages__",
    ]
    ```
  - For foundational libraries that nearly every part of the tree may use (for
    example, base libraries like `//zircon/system/ulib/fbl`), use
    `["//:__subpackages__"]`. This is especially useful when the GN target is
    public (`"*"`): Bazel and GN visibility then match.
  - Needing a `# @bazel2gn:raw_overwrite:` annotation on `visibility` is a
    smell. Avoid it unless something about the path really is GN-specific.

---

## Decision Matrix Summary

| Consumers | Visibility Rule |
| :--- | :--- |
| Only current package | Omit `visibility` (default private) |
| Current package + tests / sub-tools | `[":__subpackages__"]` |
| All within same subsystem (`//src/foo/...`) | `["//src/foo:__subpackages__"]` |
| Assembly Input Bundles | `["//bundles/assembly:__subpackages__"]` |
| Test targets for parent `test_suite` | `["//tools:__pkg__"]` (or parent package) |
| 2-3 distinct subsystems | `["//src/subsys1:__subpackages__", "//src/subsys2:__subpackages__"]` |
| 1-2 specific sibling packages | `["//src/foo/pkg_a:__pkg__", "//src/foo/pkg_b:__pkg__"]` |
| Foundational library used tree-wide | `["//:__subpackages__"]` |
| Same long list repeated across packages | A shared `package_group` |
| SDK atom | Same as any other target (the `_idk` atom target gets the visibility it needs automatically) |

---

## Verification

After applying visibility to your migrated target in `BUILD.bazel`:

1.  **Format Code First:** Run buildifier / code format to ensure formatting is
    clean:
   ```bash
   fx format-code
   ```

2.  **Verify Target and All Dependents in One Build Command:** Build the
    migrated target and all known dependent targets together in a single `fx
    build` command to ensure visibility is accepted across all consumers while
    minimizing build time:
   ```bash
   fx build @//path/to/package:target_name @//path/to/consumer1:target1 @//path/to/consumer2:target2
   ```
