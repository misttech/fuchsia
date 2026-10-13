# Determining Appropriate `Test:` Footers for Bazel Migration CLs

This document defines guidelines for selecting, consolidating, and formatting `Test:` footers in Git commit messages for GN-to-Bazel migration changelists (CLs).

---

## 1. Overview

Every migration CL must include a `Test:` footer documenting the exact commands used to verify the build and tests. These commands serve as a reproducible record for reviewers and automated verification harnesses (such as Planter).

---

## 2. Core Guidelines

### A. Consolidate to At Most 3 Lines

During multi-directory migrations or iterative development, dozens of per-package incremental commands (e.g., individual `fx build` or `fx bazel build` invocations) are often executed.
- **Do not list per-package incremental commands.**
- Consolidate verification commands down to at most **3 `Test:` lines** total.

### B. Combine Targets and Prefer Higher-Level or Dependent GN Targets (Within Reason)

Where possible, minimize the number of `Test:` commands while maximizing verification coverage:
- **Prefer a GN target that depends on the Bazel target:** Ideally specify a GN target that depends on the migrated Bazel target. This gives stronger confidence that the Bazel target is wired into the build tree somewhere. Only specify a Bazel target (e.g., `@//...` or `fx bazel ...`) if there is no GN option (e.g., `fx build //...` or `fx test //...`, both of which may have `--host`) that covers the target(s).
- **Specify multiple targets in a single `fx` command:** Combine targets that share the same command and configuration (for example, all host test targets or all GN targets for Fuchsia/target) onto a single `fx ...` command line rather than listing separate commands:
  - **Preferred:** `Test: fx test --host //src/subsystem:subsystem_tests_gn_target @//src/bar:bar_test`
- **Prefer higher-level targets (within reason):** When available, specify a higher-level target that covers multiple migrated targets (such as a GN `group("tests")` that wraps a `bazel_test_suite`, or an area `test_suite()` when no GN option exists). Do **not** specify unscoped or overly broad targets such as `fx build` (with no target arguments) or `fx build //sdk`, as they pull in too many unrelated targets. Choose the narrowest higher-level target that covers the migrated targets.
- **Building and testing Bazel targets directly & cross-platform consistency:**
  - When you need to build or test Bazel host targets directly, you can use `fx build --host @//...` or `fx test --host @//...`:
    - `fx build --host @//...` is useful for building both GN and Bazel targets in a single command when there is no GN target that depends on the Bazel target.
    - `fx test --host @//...` also works (e.g., to run specific Bazel host tests or combine GN and Bazel host tests in one command), though it requires that a GN target that depends on the Bazel test target is in `host_labels` in `args.gn` (`gn args`).
  - **Be careful with `@//` (Bazel) vs. `//` (GN) labels:** When specifying `fx test` commands, ensure that each target specified is the actual Bazel or GN target intended (i.e., begins with `@//` for a Bazel target or `//` without `@` for a GN target). This also applies to `fx build`, though we generally prefer to specify a GN target that includes the Bazel target.
  - **Note:** `fx build @//...` and `fx test @//...` do **not** currently work for Fuchsia/target targets.
  - When a target needs to be built or tested directly in Bazel (and have `Test:` lines) for both Bazel host and Fuchsia/target, it is better to be consistent and use `fx bazel ...` for both (unless there is a specific reason to mix mechanisms, in which case explain why in the commit message body).

### C. Record Only Executed and Passing Commands

- Every command in the `Test:` footer must have actually been run and exited with status `0`.
- **Do not include speculative commands.** For example, do not list `fx test` commands if tests were only built but not executed (e.g. when an emulator or physical device was not available).

### D. Quote Toolchain Parentheses for Bash Safety

Automated verification tools (such as Planter's test verifier) execute `Test:` lines via `bash -c`.
- Any GN label containing toolchain specifications with parentheses must be properly shell-quoted:
  - **Correct:** `Test: fx build '//path/to:target(//build/toolchain:host_x64)'`
  - **Incorrect:** `Test: fx build //path/to:target(//build/toolchain:host_x64)` (causes bash syntax error near unexpected token `(`)

### E. Ensure Targets Are Configured in the Build Graph

- The paramount requirement is that **every listed `Test:` command was actually run and passed**.
- Targets do not necessarily need to be part of the default build graph, but if they are not, they **must be explicitly added to the active build graph** (for example, via `fx set ... --with //path/to:tests` or `args.gn`) so they can actually be built and tested.
- Never record commands against targets that were not configured into the build graph, as running `fx build` against unconfigured GN labels will fail (e.g. with `Unknown GN label`).

### F. Do Not Wrap `Test:` Lines

- `Test:` lines may exceed the 72-character commit message line limit in order to fit the entire command and target label(s).
- A `Test:` line **MUST NOT** be broken or wrapped across multiple lines.

### G. `bazel2gn` Synchronization Verification

- When a migration synchronizes targets back to `BUILD.gn` via `bazel2gn`, record `bazel2gn` sync verification using the single canonical line:
  - `Test: fx host-tool shac check --only verify_bazel2gn`
- Place this line **last** after all build and test `Test:` lines.
- Do not list `Test: fx bazel2gn` (which mutates files rather than checking for drift) or per-directory `verify_bazel2gn` commands.
