# Selecting and Grouping Directories for Bazel Migration CLs

This document defines the target guidelines for scoping GN-to-Bazel migration changelists (CLs) in Fuchsia, specifically when to migrate a single directory, when to group multiple directories into one CL, and when to split migrations into separate or stacked CLs. In practice, these rules are primarily intended to be applied by automated migration tooling and agents rather than requiring developers to manually evaluate them.

Properly scoping migration CLs is important for several reasons:
- Easier on reviewers:
  - Reviewers only need to understand a single area of the codebase.
  - Patterns and target conventions are likely to be consistent within a given area.
  - Reviewers only need to reason about a small number of targets and a single target category at a time. For example:
    - Rust targets and tests differ from C++ targets.
    - Host tools differ from libraries, especially cross-platform libraries where reviewers must verify cross-platform compatibility and ensure tests are defined for both host and Fuchsia.
- Easier to describe and explain in the commit message.
- Avoids having to describe too many different things in the commit message.
- Avoids tying up simple migrations in a CL with more complex migrations, preventing delays in landing simpler migrations that may in turn be blocking other migrations.

---

## 1. Core Scoping Principles

### A. Default to One Complete Directory per CL

The standard unit of a GN-to-Bazel migration is a **single directory** (`BUILD.gn` to `BUILD.bazel`):
- Aim to migrate **all** targets defined in that directory's `BUILD.gn` (including libraries, binaries, and unit tests) and address remaining GN references so that the `BUILD.gn` file is deleted, making the migration of the directory complete rather than partial (see [`general_guidelines.md`](/build/beads/references/migration/general_guidelines.md)).
- If a library target in the directory is only used by one (or very few) GN targets in a neighboring directory within the same subsystem, and that consumer has few dependencies, consider migrating the consumer directory in the same CL so that `bazel2gn` synchronization and `BUILD.gn` are no longer needed in the library's directory (see [Section 2.A](#a-a-target-and-its-sole-or-tightly-coupled-same-area-consumer)).
- Group multiple directories into a single CL only when they share a tight logical relationship and target type (typically **at most 3-5 directories**). Note that grouped directories do not need to have a dependency relationship; for example, sibling directories within an area or siblings in generic directories like `//sdk/lib` can be grouped when they follow the patterns in [Section 2](#2-good-patterns-when-to-group-multiple-directories).

---

## 2. Good Patterns: When to Group Multiple Directories

Grouping multiple directories into a single CL is appropriate when all of the following hold:
1. **Same subsystem/area (or shared generic parent directory):** The directories belong to the same subsystem/area (or a generic parent directory such as `//sdk/lib`). While identical `OWNERS` files are not strictly required, `OWNERS` files are a useful indicator of areas and subtrees.
2. **Same target type:** The directories contain targets of the same type. For example, do **not** migrate a host tool alongside unrelated libraries, a FIDL library alongside non-FIDL targets, or unrelated C++ and Rust targets in the same CL.
3. **Single cohesive unit of review:** The directories match one of the specific grouping patterns below.

> **Important:** Do not combine multiple grouping patterns or tack on extra directories. If multiple directories already have a relationship under one of the cases below (such as a target and its sole consumer, or a target and its helper library), stop there and do **not** also add sibling directories or other directories in the same area to the CL.

### A. A Target and Its Sole (or Tightly Coupled) Same-Area Consumer

When migrating directory `A`, if `fx gn refs` shows that directory `B` in the same subsystem is the only remaining GN consumer of `A` and `B` has minimal dependencies, migrate `A` and `B` together.
- **Why:** Eliminates the last GN dependency on `A`, avoiding `bazel2gn` boilerplate and allowing `A/BUILD.gn` to be cleaned up or deleted.

### B. A Root Target and Its Private/Internal Helper Packages

When a top-level package depends on one or more small helper subdirectories within the same area that exist primarily to support that target:
- **Subject format:** Reference the top-level dependent target or directory:
  ```none
  [bazel_migration][developer] //src/developer/ffx/lib/isolate
  ```
  *(when migrating `isolate` alongside internal helper libraries in the same subsystem)*

### C. A Small, Cohesive Subsystem or Sibling Group

When a small number of sibling or child directories (`<= 3-5` directories) of the same target type form a single logical library family under a shared area tag, or are simple sibling libraries in an area or generic parent directory (such as `//sdk/lib`), and have similar build structures:
- **Subject formats:**
  ```none
  [bazel_migration][power] power_broker libraries
  ```
  ```none
  [bazel_migration][starnix] selinux and ebpf libraries
  ```
  ```none
  [bazel_migration][starnix] //src/starnix/lib: selinux, ebpf
  ```

### D. A Package and Its Direct Test Subdirectories

When a package's unit or integration tests live in direct child directories (such as `//<dir>/tests` or `//<dir>/test`) and exercise the targets being migrated in `//<dir>`, migrate them in the same CL so the target and its tests move to Bazel together.

---

## 3. Inappropriate Groupings: When to Split into Separate CLs

When a target has unmigrated upstream dependencies, **do not automatically sweep the entire transitive dependency closure into a single CL**. Instead, use the [`analyzing-migration-dag`](/build/beads/.agent/skills/analyzing_migration_dag/SKILL.md) skill to identify topological waves and split the work into separate precursor CLs (or a stack of focused CLs), grouping directories only when they follow the good patterns in the previous section.

### A. Directories Across Different Areas

Do **not** combine directories from different subsystems or areas in a single CL, even if one transitively depends on another (inspecting the alphabetized list of directories makes cross-area groupings easy to spot).
- **Example to avoid:** Mixing targets under `//src/connectivity/...` and `//src/diagnostics/...` in one CL.
- **Why:** Each area has distinct domain owners (as reflected in its `OWNERS` files), conventions, and review concerns. Cross-area CLs require multiple unrelated reviewers and obscure which area tag belongs in the commit subject.

### B. General-Purpose Libraries and Indirect Dependencies

Foundational or general-purpose libraries should be migrated separately from a downstream consumer when the consumer is just one of many users of the library (meaning there is no close relationship between them) or when they are in different areas. (Such libraries do not necessarily need to be in standalone single-directory CLs; they may still be grouped with other directories when appropriate under [Section 2](#2-good-patterns-when-to-group-multiple-directories), such as with sibling libraries in the same area.)

Similarly, **indirect (transitive) dependencies** should generally be migrated in separate precursor CLs rather than bundled with the top-level target being migrated, especially when they are not in the same area. Never migrate an isolated target in an upstream dependency directory just to unblock a downstream consumer (see [`general_guidelines.md`](/build/beads/references/migration/general_guidelines.md)); migrate the upstream directory completely in its own CL.

### C. `third_party/` Directories

Migrations within any `third_party/` directory (whether under `//third_party/` or nested elsewhere in the tree) are often complex and **must be handled in separate CLs** from first-party (`//src/...`, `//tools/...`, `//sdk/...`) migrations, and usually separate from other `third_party/` directories:
- `third_party/` directories frequently involve external git submodules, file exports, generated headers, C/C++ and Rust FFI linkage nuances, shared vs. static library variants, or roller-maintained files.
- Separate different third-party projects into separate CLs. Closely coupled directories within the *same* simple third-party project (such as a C library and its `rust/` bindings subdirectory) may be grouped together in a dedicated CL for that third-party project.

### D. Host Tools vs. Libraries and Consumers

Host tools should always be migrated using the [`migrating-host-tool-to-bazel`](/build/beads/.agent/skills/migrating_host_tool_to_bazel/SKILL.md) skill in a **separate CL** from:
1. Upstream libraries in other areas that the host tool depends on, and
2. Downstream targets in other areas that invoke or depend on the host tool. However, when possible, GN targets that invoke the tool should be updated in the host tool's migration CL to use the Bazel-defined binary (via `bazel_host_tool()`) so that the binary target definition in `BUILD.gn` can be deleted (see [`general_guidelines.md`](/build/beads/references/migration/general_guidelines.md)).

---

## 4. Warning Signs That a CL Scope Is Too Large or Diverse

Attempting to write the commit message according to [`//build/beads/references/migration/commit_message/guidelines.md`](/build/beads/references/migration/commit_message/guidelines.md) and its peer files provides a reliable litmus test for CL scope. In particular, examining the alphabetized lists of directories and targets in the commit message is helpful for spotting when a CL spans unrelated areas (as in [Section 3.A](#a-directories-across-different-areas), such as mixing `//src/connectivity/...`, `//src/developer/...`, and `//src/diagnostics/...`). If you encounter any of the following difficulties while drafting the commit message, the CL is likely too large or diverse and should be split.

### A. Subject Line and `[<area>]` Tag Friction

Per [`commit_message/guidelines.md`](/build/beads/references/migration/commit_message/guidelines.md), the subject line must follow `[bazel_migration][<area>] <target-or-directory-reference>` (aiming for `<= 50-65` characters):
- **Mismatched `[<area>]` tag and directories:** All directories in a CL should be from the same area. If the `[<area>]` tag matches some directories in the CL but not all of them, or worse, does not match the directory named in the subject line (for example, `[bazel_migration][pkg] //src/lib/storage/vfs`), the directories do not belong in the same CL.
- **Subject directory is an unrelated leaf dependency:** If the directory in the subject line does not depend on the other directories in the CL, the grouping is inverted or too broad.
- **No concise multi-directory summary fits:** If multiple directories cannot be concisely named in the subject line using one of the three standard patterns in [`commit_message/guidelines.md` Section 2.D](/build/beads/references/migration/commit_message/guidelines.md) (root dependent target, subsystem/library family, or short list of directories under a shared area), or if you feel forced to add an `"Also migrated in this change:"` list at the top of the commit body, the CL scope is too large.

### B. Blanket Statements in the Body Become Inaccurate or Vague

Per [`commit_message/guidelines.md` Section 4](/build/beads/references/migration/commit_message/guidelines.md), the commit body should mention what was **not** migrated, what remains defined in `BUILD.gn`, whether `bazel2gn` is used (and why), and any non-trivial conversion details. When a CL groups too many or overly diverse directories, describing these details cleanly becomes difficult:
- **Summary bullets are not true for all directories:** A summary paragraph such as *"Sync library and binary targets to `BUILD.gn` via `bazel2gn` for remaining GN consumers; keep `bazel_test_suite` and `group("tests")` above the sentinel"* works well for a small number of uniform targets, but becomes factually inaccurate when a CL mixes diverse targets (such as a `third_party/` directory that does not use `bazel2gn` or a directory with unmigrated GN targets).
- **Inconsistent action verbs across bullets:** Describing some directories with *"Migrate ..."* and another directory with *"Add Bazel targets in `//...` needed by ..."* indicates that the latter may not have been properly migrated and should be handled separately (or escalated to the user).

### C. `Test:` Footers Cannot Be Consolidated into `<= 3` Focused Lines

Per [`determining_appropriate_test_footer.md`](/build/beads/references/migration/commit_message/determining_appropriate_test_footer.md), `Test:` footers must be consolidated to **at most 3 lines**, preferring narrow higher-level GN targets or area `test_suite()` targets while avoiding unscoped commands like bare `fx build`:
- If the migrated directories are so scattered that no narrow higher-level target or area `test_suite()` covers them, forcing you to either chain many unrelated `//...` test labels across subsystems or fall back to an overly broad build target, the CL combines too many unrelated areas.

### D. Sprawling `Bazel-Migration-Target:` Footer Block

While multiple `Bazel-Migration-Target:` lines are allowed (see [`determining_appropriate_bazel_migration_target_footer.md`](/build/beads/references/migration/commit_message/determining_appropriate_bazel_migration_target_footer.md)), a footer block listing 10 or more targets across 4-5 distinct top-level directory prefixes is a strong indicator that the CL should be broken up.
