# General Guidelines for Bazel Migrations

This document defines general principles and best practices for migrating build targets from GN (`BUILD.gn`) to Bazel (`BUILD.bazel`) in Fuchsia.

---

## 1. Perform Complete, Proper Migrations

Every directory migrated to Bazel should undergo a complete, proper migration rather than a partial conversion that only migrates a subset of targets to unblock a downstream consumer:
- **Migrate all targets in a directory (avoid partial directory conversions):**
  A complete migration means migrating all targets defined in that directory's `BUILD.gn` (including libraries, binaries, and unit tests). Aim to touch a `BUILD.bazel` file only once. Migrating only some targets in a multi-target `BUILD.gn` often leads to awkward mixed build states and complex workarounds. While converting a partial set of targets may occasionally be necessary when specific targets in a directory are blocked, it should not be the default choice or how automated agents migrate directories.
- **Target and attribute parity:** Migrate all targets with equivalent properties in `BUILD.bazel`, including shared vs. static library differentiation, `target_compatible_with` constraints, visibility, unit tests, and lint configurations.
- **Keep `BUILD.bazel` and `BUILD.gn` synchronized:** When a `BUILD.gn` file must remain for unmigrated GN consumers, keep the Bazel and GN target definitions in sync using `bazel2gn` (or IFTTT `LINT` comments such as `# LINT.IfChange` / `# LINT.ThenChange(BUILD.gn)` when `BUILD.gn` is too complex for `bazel2gn`, as in `//third_party/zstd/BUILD.bazel`).
- **Do not cherry-pick single targets from upstream dependency directories:**
  When migrating target `foo`, never reach into an unmigrated upstream directory `bar` to migrate only the single target or simplified definition that `foo` happens to need. Doing so leaves `bar` in an incomplete state and creates maintenance churn. Instead, directory `bar` should be migrated completely on its own (typically as a separate precursor CL; see [`scoping_changelists.md`](/build/beads/references/migration/scoping_changelists.md)).
  - Note: This is distinct from deliberate, global build system exceptions (such as temporarily deferring specific macro types across the tree) where a directory is migrated according to an established global rule rather than cherry-picking targets to satisfy a single consumer.
  - If an upstream dependency cannot be properly migrated, stop and report the blocker to the user for review.

---

## 2. Evaluate Remaining GN Consumers to Eliminate `BUILD.gn` and `bazel2gn`

A migration of a directory is only truly complete when its `BUILD.gn` file is deleted. Do not passively accept that a target has a remaining GN reference and therefore must stay in `BUILD.gn` or use `bazel2gn`. Automated tooling and agents should systematically inspect downstream GN references (`fx gn refs $(fx get-build-dir) "//<dir>/*"`) before generating `bazel2gn` configurations to see if small, targeted updates allow deleting the legacy GN targets or the entire `BUILD.gn` file:

1. **Host tool binaries referenced in GN actions:**
   - If a host binary target in `BUILD.gn` is only referenced by GN targets via `tools = [ ... ]` (for example, in a `.gni` template or `compiled_action`), migrate the host tool using the [`migrating-host-tool-to-bazel`](/build/beads/.agent/skills/migrating_host_tool_to_bazel/SKILL.md) skill and switch the GN reference to a `bazel_host_tool()` wrapper target. This eliminates the legacy GN binary target.
2. **Test-only `BUILD.gn` remnants:**
   - If all production targets in a directory are migrated and no external GN targets depend on them, do not keep `BUILD.gn` solely for a `group("tests")` target that depends on a `bazel_test_suite()`. Instead, add the Bazel test dependency to an appropriate ancestor Bazel `test_suite()` or ancestor `bazel_test_suite()` target so `BUILD.gn` can be deleted entirely.
3. **Single-consumer libraries in the same subsystem:**
   - If a library target is only used by one (or very few) GN targets in a neighboring directory within the same subsystem, and that consumer has few dependencies, consider migrating the consumer directory in the same CL so that `bazel2gn` synchronization is unnecessary in the library's directory (see [`scoping_changelists.md`](/build/beads/references/migration/scoping_changelists.md)).

If remaining GN dependencies cannot be easily migrated or updated to Bazel wrappers, report them to the user for review.

---

## 3. CQ Usage for Stacked Migration CLs

When splitting a multi-directory migration into a stack of dependent CLs (see [`scoping_changelists.md`](/build/beads/references/migration/scoping_changelists.md)), you do not need to run Commit Queue (CQ) on every intermediate commit. Per the [`fuchsia-gerrit-cli`](/.agents/skills/fuchsia-gerrit-cli/SKILL.md) guidelines, only trigger `Commit-Queue+1` on the **first and last CLs** in a patch stack.
