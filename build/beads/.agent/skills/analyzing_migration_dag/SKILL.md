---
name: analyzing-migration-dag
description: >-
  Analyzes the recursive GN-to-Bazel dependency and unit-test DAG for a Fuchsia
  target or BUILD.gn package, detects hard blockers, and computes deterministic
  topological migration waves using Kahn's in-degree algorithm. Use before
  migrating any target with migrating-host-tool-to-bazel or
  migrating-fidl-to-bazel to determine the exact upstream migration order and
  stacked CL plan.
---

# Analyzing GN-to-Bazel Migration Dependency DAG

A Fuchsia target can only be migrated from GN to Bazel once all of its upstream
dependencies are available in Bazel and compatible with the target platform
(e.g., host or Fuchsia).

This skill defines the standard **Recursive Topological Dependency Analysis &
Stacked-CL Planning Workflow** for discovering all unmigrated upstream
dependencies (including unit tests), detecting hard blockers early, and sorting
targets into deterministic migration waves before authoring `BUILD.bazel` files.

## Related Skills

- **Per-Target Host Tool & Library Migration:**
  [`migrating-host-tool-to-bazel`](../migrating_host_tool_to_bazel/SKILL.md)
- **Per-Target FIDL Library Migration:**
  [`migrating-fidl-to-bazel`](../migrating_fidl_to_bazel/SKILL.md)
- **Syncing Migrated Libraries Back to GN:**
  [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md)

---

## Core Principles

1. **Full Recursive Upstream Scan (Production + Unit Tests):**
   - Before writing any `BUILD.bazel` file, recursively scan the target's entire
     upstream GN dependency graph (`deps`, `public_deps`, `data_deps`,
     `non_rust_deps`, `proc_macro_deps`, `libraries`, `embed`, `args_deps`,
     `plugin_deps`, `host_deps`, and `test_deps`).
   - **Unit Test Inclusion Rule:** By default, the analyzer includes the tests
     in the root target's package family (`//dir/BUILD.gn`,
     `//dir/tests/BUILD.gn`, and `//dir/test/BUILD.gn`) that *exercise* a
     target being migrated: tests that depend on it (directly or through other
     test targets, e.g. `fuchsia_unittest_component` -> `:foo_test` -> `:foo`)
     or that recompile the same `sources` in the same BUILD.gn. Aggregator
     groups (`group("tests")`, `group("host_tests")`) are never followed, so
     tests of child directories and downstream consumers are not pulled in.
     Upstream libraries (`rustc_library`) can keep their unit tests in GN via
     `bazel2gn` if their `test_deps` are not yet migrated, so only their
     production dependencies are scanned. Pass `--include-upstream-tests` to
     also expand `test_deps` and tests for every unmigrated upstream package.

2. **Fail-Fast Hard Blocker Detection:**
   - Detect out-of-scope or unsupported transitive dependencies (`HARD_BLOCKER`)
     across the full recursive graph before starting migration work. Examples
     include:
     - Unmigrated `//third_party/*` repositories without a `BUILD.bazel` file or
       an entry in `//build/tools/bazel2gn/third_party_target_map.json`.
     - Third-party Protobuf libraries requiring dynamic `go_proto_library`
       codegen in Bazel (e.g., `//third_party/luci-go/...`).
     - Custom `.gni` codegen templates without Bazel equivalents.

3. **Kahn's Algorithm (In-Degree Topological Sort):**
   - Prune dependencies that are already migrated and platform-compatible in
     Bazel (`MIGRATED_READY`).
   - For every remaining unmigrated node, compute **`in_degree(node)`** = the
     number of unmigrated direct dependencies of `node`.
   - Group targets into deterministic **Migration Waves**:
     - **Wave 0 (`in_degree == 0`):** Leaf unmigrated targets whose upstream
       dependencies are 100% already in Bazel ("free-to-migrate" immediately).
     - **Wave $k$ ($k \ge 1$):** Targets whose `in_degree` drops to `0` after
       Waves $0 \dots k-1$ are migrated.
     - **Final Wave:** The top-level root target and its unit tests.

4. **Stacked CL Strategy (Max 4-5 Targets per CL):**
   - Plan multi-target migrations as **stacked Git commits on a single local Git
     branch (`migrate-<target_name>`)** ordered from Wave 0 up to the root
     target, sharing the same `Bug: <id>` footer.
   - Keep each commit/CL small and reviewable by limiting each CL to **at most
     4-5 targets**.
   - While this skill determines the **migration order**, the migration of each
     individual target must strictly follow
     [`migrating-host-tool-to-bazel`](../migrating_host_tool_to_bazel/SKILL.md)
     (or [`migrating-fidl-to-bazel`](../migrating_fidl_to_bazel/SKILL.md)) and
     [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md).

---

## Workflow Steps

### Step 1: Run the Automated DAG Analyzer Script

Run [`scripts/analyze_migration_dag.py`](scripts/analyze_migration_dag.py) from
the Fuchsia checkout root on the target GN label or `BUILD.gn` path:

```bash
# Analyze a specific GN target with full transitive recursion (default --max-depth -1):
python3 build/beads/.agent/skills/analyzing_migration_dag/scripts/analyze_migration_dag.py \
  "//path/to/dir:target_name"

# Analyze all targets in a package's BUILD.gn:
python3 build/beads/.agent/skills/analyzing_migration_dag/scripts/analyze_migration_dag.py \
  "//path/to/dir/BUILD.gn"

# Fast shallow iteration (limit cross-package expansion to 1 or 2 hops):
python3 build/beads/.agent/skills/analyzing_migration_dag/scripts/analyze_migration_dag.py \
  "//path/to/dir:target_name" --max-depth 2

# Include upstream package unit tests and test_deps across the full DAG:
python3 build/beads/.agent/skills/analyzing_migration_dag/scripts/analyze_migration_dag.py \
  "//path/to/dir:target_name" --include-upstream-tests
```

#### Understanding the Script Output

1. **Node Classification Summary:**
   - `MIGRATED_READY`: Target already exists in `BUILD.bazel` (or `third_party_target_map.json`,
     `//third_party/rust_crates`, or `//third_party/golibs`). Pruned from further
     recursion (any platform constraint adjustments are handled during migration).
   - `UNMIGRATED_IN_SCOPE`: In-tree target (`//src/...`, `//tools/...`,
     `//sdk/...`, `//zircon/...`, `//build/...`, `//scripts/...`) that needs to
     be migrated to Bazel. Subdivided into:
     - **Unblocked (`[MIGRATABLE]`):** Can be migrated now without hitting any
       hard blockers.
     - **Transitively Blocked (`[BLOCKED]`):** Waits directly or transitively on
       one or more `HARD_BLOCKER` targets.
   - `HARD_BLOCKER`: Out-of-scope `//third_party/*` target or unsupported custom
     GN template.
2. **Hard Blockers Detected:** Lists every `HARD_BLOCKER`, its cross-package hop
   depth, and the reason it is blocked.
3. **Explicit CL-by-CL Migration Order (Targets in Same Step Packed into Same CL):**
   Groups unmigrated `BUILD.gn` packages into reviewable stacked CLs ($\le 5$
   packages per CL) in strict topological and subsystem-affinity order so every
   `CL k` only depends on `CL < k`:
   - **Phase A (`CL 1` - `CL M`):** Unblocked in-tree targets that can be
     migrated and landed immediately without waiting on any hard blockers.
   - **Phase B (`CL M+1` - `CL N`):** Hard blockers followed by transitively
     blocked downstream targets up to the root target.
   - **Upstream Unit-Test (`test_deps`) Notes:** Lists any extra `test_deps`
     targets or unit-test dependency cycles across CLs.
4. **Topological Migration Waves (Kahn's In-Degree Reference):** Lists `Wave 0`,
   `Wave 1`, ..., `Wave N`, showing each target's GN rule type, hop depth,
   initial unmigrated `in-degree`, and exact unmigrated prerequisites (`waits on`).
5. **Direct Upstream Dependencies Already Migrated (Pruned):** Confirms which
   upstream dependencies are already available in Bazel.

### Step 2: Check Toolchain-Specific GN Graph & Downstream Referrers

Supplement the static DAG analysis with `fx gn desc` and `fx gn refs` to inspect
toolchain-specific conditionals and identify all downstream GN targets that
depend on the package:

```bash
# Inspect host toolchain dependency tree in GN:
fx gn desc $(fx get-build-dir) "//path/to/dir:target_name(//build/toolchain:host_x64)" deps --tree

# Inspect all downstream GN consumers referencing this directory:
fx gn refs $(fx get-build-dir) "//path/to/dir/*"
```

### Step 3: Present the Migration Plan & Wait for User Approval

Before creating any Git branches or editing `BUILD.bazel` / `BUILD.gn` files:

1. Present the **Node Classification Summary**, **Hard Blocker Chains** (if any),
   **Explicit CL-by-CL Migration Order (Phase A & Phase B)**, and **Downstream
   GN Referrers** to the user.
2. If `HARD_BLOCKER` nodes exist:
   - Clearly highlight the blocker chain preventing the root target from being
     migrated.
   - Identify whether **Phase A** (`CL 1` - `CL M`, unblocked targets) should
     be migrated incrementally now to unblock shared upstream libraries, or if
     the target should be skipped until the blocker is resolved.
3. **Stop and wait for explicit user confirmation** before executing the
   migration CLs.

### Step 4: Execute Migration Wave-by-Wave

Once approved by the user, migrate targets in topological order starting from
**Wave 0 (`in_degree == 0`)**:

1. For each target in the current wave/CL batch, follow
   [`migrating-host-tool-to-bazel`](../migrating_host_tool_to_bazel/SKILL.md)
   (or [`migrating-fidl-to-bazel`](../migrating_fidl_to_bazel/SKILL.md) for FIDL
   targets).
2. Whenever an upstream library in Wave $k$ is still referenced by unmigrated GN
   targets in Wave $> k$ (or external GN consumers), sync the library back from
   Bazel to GN using [`syncing-bazel-to-gn`](../syncing_bazel_to_gn/SKILL.md)
   (`fx bazel2gn -d //path/to/dir`), including its Verify step.
3. Commit each wave batch (at most 4-5 targets) as a stacked commit on the
   single migration branch before proceeding to the next wave.
