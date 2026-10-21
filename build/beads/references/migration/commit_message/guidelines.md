# Commit Message Guidelines for Bazel Migration CLs

This document defines conventions for formatting Git commit messages (subject line, body, and footers) for GN-to-Bazel migration changelists (CLs) in Fuchsia.

---

## 1. Title / Subject Line Format

The subject line of a migration CL must follow a structured format:

```none
[bazel_migration][<area>] <target-or-directory-reference>
```

### Components

1. **`[bazel_migration]` Prefix:**
   - Every migration CL title **must** begin with `[bazel_migration]`.
   - This tag identifies the change as migrating targets from GN to Bazel.

2. **Area Tag (`[<area>]`):**
   - Follow immediately with the tag that would normally be used for changes to that part of the codebase (e.g. `[drivers]`, `[storage]`, `[starnix]`, `[media]`, `[developer]`).
   - Do **not** place a space between `[bazel_migration]` and the area tag: `[bazel_migration][<area>]`.

3. **Space and Target/Directory Reference:**
   - Follow the area tag with a single space and the canonical label of the directory or target being migrated.
   - The label **must** begin with `//` (e.g., `//path/to/the/ffx/lib/package`, never `path/to/the/ffx/lib/package`).
   - **Do NOT begin this part of the title with "Migrate "** (e.g., use `//src/...`, never `Migrate //src/...`). The `[bazel_migration]` tag already communicates that the change is a migration.

---

## 2. Referencing What Was Migrated

Depending on the scope of the migration, use the following conventions for the reference following `[bazel_migration][<area>] `:

### A. Entire Directory Migrated

When migrating all targets defined in a directory's `BUILD.gn`:
- Use the directory label starting with `//`.
- **Format:** `[bazel_migration][<area>] //<dir_path>`
- **Example:**
  ```none
  [bazel_migration][starnix] //src/starnix/lib/selinux
  ```

### B. Single Target in a Directory

When only one target in a `BUILD.gn` file is migrated:
- Specify the full target label including the target name.
- **Format:** `[bazel_migration][<area>] //<dir_path>:<target_name>`
- **Example:**
  ```none
  [bazel_migration][storage] //src/storage/lib/vfs:vfs
  ```

### C. Multiple but Not All Targets in a Directory

When multiple targets (but not all targets) within a single `BUILD.gn` file are migrated:
- Use the directory label and append `" targets"` to the end of the line.
- **Format:** `[bazel_migration][<area>] //<dir_path> targets`
- **Example:**
  ```none
  [bazel_migration][media] //src/media/audio/lib targets
  ```

### D. Multiple Directories Migrated at Once

When a migration spans multiple directories, determine an appropriate way to reference all of them concisely:

1. **Reference the dependent / root target:**
   Use the label of the top-level directory or target that depends on the other migrated directories.
   - **Example:**
     ```none
     [bazel_migration][developer] //src/developer/ffx/lib/isolate
     ```
     *(when migrating `isolate` alongside helper libraries it depends on)*

2. **Group by subsystem or library family:**
   If a dependent target doesn't naturally represent the change, group the directories by conceptual name or subsystem:
   - **Example:**
     ```none
     [bazel_migration][power] power_broker libraries
     ```

3. **List directories under a shared area:**
   If all migrated directories share the area tag (e.g. all under `//src/starnix`), list the specific directory names:
   - **Examples:**
     ```none
     [bazel_migration][starnix] selinux and ebpf libraries
     ```
     ```none
     [bazel_migration][starnix] //src/starnix/lib: selinux, ebpf
     ```

If a good summary cannot be determined or the list of directory names is long, that may be a sign that the scope of this single change is too large and should be broken up, ideally along boundaries that match the order of suggestions above (e.g. by dependent targets/subtrees, by logical groupings, or by directory hierarchies).

---

## 3. Line Length Requirements

Follow the line length limits from the [Fuchsia commit message style guide](/docs/contribute/commit-message-style-guide.md):
- **Subject line (Line 1):** Aim for **50 characters or fewer** when possible, and keep it **65 characters or fewer** (or at most 72 characters). Omitting redundant verbs like `"Migrate "` helps keep full `//...` labels within this limit. However, if necessary, **clarity and accuracy are more important than line length** for the subject line.
- **Blank line (Line 2):** Always leave a single blank line between the subject line and the body.
- **Body lines:** Wrap all prose body lines at **72 characters or fewer**.
- **Footer line length exceptions:** `Test:` and `Bazel-Migration-Target:` footer lines may exceed the 72-character limit to fit the entire command or target label. These lines **MUST NOT** be broken into multiple lines.

---

## 4. Commit Message Body

The commit body must clearly explain the scope and details of the migration so human reviewers and automated checks can verify the change against the diff:

1. **Opening Summary & Migration Skill(s) Used:**
   - Start with a sentence summarizing what was migrated from GN to Bazel and, if applicable, which migration skill(s) were used:
     - **Single skill:** `Migrate <target-or-directory-description> from GN to Bazel using the "<skill-name>" skill.`
     - **Multiple skills:** `Migrate <target-or-directory-description> from GN to Bazel using the "<skill-1>" and "<skill-2>" skills.`
   - Adapt `<target-or-directory-description>` to accurately describe whether a single target, a subset of targets in a package, an entire directory/package, or multiple directories were migrated.

2. **Migrated & Unmigrated Target Details:**
   - List the specific directories or targets converted to Bazel (`BUILD.bazel`). Always alphabetize lists of directory paths and target labels.
   - Explicitly identify any targets in the directory that were **not** migrated in this CL and explain why (e.g., blocked on unmigrated dependencies or GN-only templates).

3. **GN & `bazel2gn` Status:**
   - State whether `BUILD.gn` was deleted or still exists, and list which hand-written targets still remain in `BUILD.gn` (for example, `group("tests")` or unmigrated targets).
   - State whether migrated targets are synchronized back to `BUILD.gn` via `bazel2gn` (and why GN consumers remain).
   - Explicitly note any targets in `BUILD.bazel` that do **not** use `bazel2gn` (such as targets marked with `# @bazel2gn:skip`).

4. **Other Non-Trivial Conversion Details:**
   - Note any non-trivial aspects of the migration, such as visibility scoping, `target_compatible_with` constraints, IDK atom registration, or changes to shared `.bzl` files outside the migrated directory.

---

## 5. Commit Message Footers

### Critical Formatting Rules

- **Single Contiguous Block:** All footers **MUST** be grouped in a single contiguous block at the very bottom of the commit message, separated from the body by a single blank line, with **no blank lines between any footers**. Any footer separated from the final block by an empty line may not be parsed as a footer by Gerrit (for example, Gerrit searches such as `hasfooter:Bazel-Migration-Target` will fail to match footers placed above a blank line).
- **No Line Wrapping on `Test:` or `Bazel-Migration-Target:`:** Both `Test:` and `Bazel-Migration-Target:` lines may exceed the 72-character line limit to fit the entire command or label, and **MUST NOT** be broken across multiple lines.
- **Do NOT Use `No-Try: true`:** Do **not** include the `No-Try: true` footer. The one exception is when uploading Planter learnings in CLs that **only** contain files in `//build/beads/planter`.

### Footer Order and Definitions

List footers in the following exact order (from most relevant to human reviewers to automated metadata):

1. **`Bug:` / `Fixed:`** *(Optional)*
   - Include a `Bug: <issue-id>` (or `Fixed: <issue-id>`) line when applicable. Omit this line if there is no relevant issue.
   - Agents **MUST** verify that any specified bug ID(s) are valid, in a Fuchsia Buganizer component, and relevant to the change.
   - See [`determining_appropriate_bug_footer.md`](/build/beads/references/migration/commit_message/determining_appropriate_bug_footer.md) for details.

2. **`Test:`** *(Required, 1 to 3 lines)*
   - List the exact commands executed to verify the build and tests (up to 3 `Test:` lines total; may exceed 72 characters and **MUST NOT** be wrapped across multiple lines).
   - For detailed rules on combining targets into a single command, selecting higher-level or dependent targets, and quoting toolchains, see [`determining_appropriate_test_footer.md`](/build/beads/references/migration/commit_message/determining_appropriate_test_footer.md).

3. **`Bazel-Migration-Target:`** *(Required, 1 or more lines)*
   - Specify the canonical `//...` label(s) of the migrated target(s) in alphabetical order to enable Gerrit tracking queries (may exceed 72 characters and **MUST NOT** be wrapped across multiple lines).
   - See [`determining_appropriate_bazel_migration_target_footer.md`](/build/beads/references/migration/commit_message/determining_appropriate_bazel_migration_target_footer.md) for guidelines on selecting target values.

4. **`Change-Id:`** *(Automated - must be the final line)*
   - Automatically added by the Gerrit `commit-msg` Git hook when creating a commit (`Change-Id: I...`).
   - **Never manually write or fabricate a `Change-Id:` line** when creating a new commit.
   - When amending an existing commit, preserve the existing `Change-Id:` line at the very bottom of the footer block (exactly once, with no blank lines between it and the preceding footers).

---

## 6. Template and Examples

### A. Commit Message Template

Use the following structure when drafting a migration commit message (omit comments starting with `#` and omit `Bug:` when no issue applies):

```none
[bazel_migration][<area>] <target-or-directory-reference>

Migrate <target-or-directory-description> from GN to Bazel using the
"<skill-name>" skill.

<Details of migrated targets, any unmigrated targets, whether BUILD.gn
was deleted or which targets remain in GN, whether/which targets are
synced via bazel2gn (or skipped), and any non-trivial conversion notes,
wrapped at <= 72 characters.>

Bug: <issue-id>
Test: <executed-verification-command-1>
Test: <executed-verification-command-2>
Bazel-Migration-Target: //<dir_path>:<target_1>
Bazel-Migration-Target: //<dir_path>:<target_2>
```
*(Note: `Change-Id: I...` will be appended automatically by the Git `commit-msg` hook immediately after the last `Bazel-Migration-Target:` line.)*

### B. Concrete Examples

#### Example 1: Directory Migration with `bazel2gn` and Remaining GN `tests` Group

```none
[bazel_migration][storage] //src/storage/lib/ptr_slice

Migrate //src/storage/lib/ptr_slice from GN to Bazel using the
"migrating-host-tool-to-bazel" and "syncing-bazel-to-gn" skills.

- Convert `ptr_slice` (`rustc_library` with `with_unit_tests = True`)
  to `BUILD.bazel`.
- Sync `ptr_slice` back to `BUILD.gn` via `bazel2gn` because reverse
  dependencies in `//src/storage` still build in GN.
- `ptr_slice_test` is exported to GN via `bazel_test_suite` and is not
  synced via `bazel2gn`. `group("tests")` remains in `BUILD.gn`.

Bug: 123456789
Test: fx test //src/storage/lib/ptr_slice:tests
Test: fx host-tool shac check --only verify_bazel2gn
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice
Bazel-Migration-Target: //src/storage/lib/ptr_slice:ptr_slice_test
```

#### Example 2: Partial Directory Migration (No `Bug:` Footer)

```none
[bazel_migration][developer] //src/developer/ffx/lib/pkg targets

Migrate the `pkg` and `pkg_config` libraries in
//src/developer/ffx/lib/pkg from GN to Bazel using the
"migrating-host-tool-to-bazel" and "syncing-bazel-to-gn" skills.

- Convert `pkg` and `pkg_config` to `BUILD.bazel` with
  `target_compatible_with = HOST_OS_CONSTRAINTS`.
- Leave `pkg_integration_test` in `BUILD.gn` as it depends on
  unmigrated test harness targets.
- Sync `pkg` and `pkg_config` to `BUILD.gn` via `bazel2gn`.

Test: fx bazel test --config=host @//src/developer/ffx/lib/pkg:pkg_test @//src/developer/ffx/lib/pkg:pkg_config_test
Test: fx host-tool shac check --only verify_bazel2gn
Bazel-Migration-Target: //src/developer/ffx/lib/pkg:pkg
Bazel-Migration-Target: //src/developer/ffx/lib/pkg:pkg_config
```
