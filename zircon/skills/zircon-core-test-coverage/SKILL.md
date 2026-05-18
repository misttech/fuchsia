---
name: zircon-core-test-coverage
description: >
  A skill for systematically analyzing Zircon syscall definitions (VDSO) and
  existing core tests to discover, write, build, and verify test coverage gaps
  such as missing handle rights validation, parameters, and error codes.
---

# Zircon Core Test Coverage Improvement Skill

This skill defines the workflow and guidelines for improving the test coverage
of the Zircon core interfaces (vDSO syscalls). It focuses on closing coverage
gaps—specifically for undocumented/untested error codes, handle rights
validation, and parameter edge cases.

## Orchestration Overview

Improving test coverage for a specific Zircon object class follows a structured
four-step process led by the Main Agent and coordinated with specialized
subagents:

```
[Discovery Subagent] ──(Gaps)──> [Main Agent] ──(Assignment)──> [Coding Subagent]
                                                                      │
                                                                 (Runs Build/Test)
                                                                      │
                                                                      ▼
[Approval / Feedback] <───(Review)─── [Critic Subagent] <──(Draft)────┘
```

---

## Phase 1: Gap Discovery (Discovery Subagent)

**Goal**: Identify exactly which syscalls, error paths, parameter validations,
and handle rights checks are undocumented or untested in the current core tests.

### Step 1. Analyze VDSO FIDL
Parse the target FIDL file located under `zircon/vdso/<object_type>.fidl` to
extract:
- Every protocol method (which maps to a `zx_<object_type>_<method>` syscall).
- For each method, list all documented errors in the `## Errors` section.
- Identify the required handle types and the required rights mentioned in the
  `## Rights` or `## Description` sections.

### Step 2. Analyze Core Tests
Locate the corresponding core test directory in
`zircon/system/utest/core/<object_type>/`.
- Search all `.cc` and `.h` files for tests calling the targeted syscalls.
- Map out which error return values are validated by existing assertions (e.g.
  `ASSERT_EQ(..., ZX_ERR_...)`).
- Look for handle rights validation. Specifically, check if there are tests
  where handles are duplicated with reduced rights and then passed to the
  syscalls to assert `ZX_ERR_ACCESS_DENIED`.

### Step 3. Report Gaps
Create a list of the precise gaps discovered:
- Untested error conditions.
- Untested handle rights (e.g., "Missing validation of `ZX_RIGHT_WRITE` on
  `zx_timer_set`").
- Untested boundary or invalid parameter inputs.

### Step 4. Verify with Kernel Implementation
Before finalizing the test plan, search for the syscall implementation under
`zircon/kernel/object/` or `zircon/kernel/lib/syscalls/` to verify that the
documented constraints (such as valid options, size limits, etc.) are actually
checked or enforced by the kernel code.
- If the kernel does not enforce a documented option (e.g., comments it out or
  ignores it), **do not** write an assertion in the test that expects
  `ZX_ERR_INVALID_ARGS` or similar failures. Asserting on unimplemented error
  checks will cause tests to fail.
- If you discover such a discrepancy, skip testing that specific parameter
  validation or document it with a clear `TODO`.

---

## Phase 2: Writing & Verification (Coding Subagent)

**Goal**: Implement standard-compliant, robust, and correct C++ tests using the
`zxtest` framework to fill the identified gaps.

### Implementation Guidelines

1.  **Style Consistency**:
    - Match the exact coding style, indentation, and structure of existing test
      files in the directory.
    - Keep helper function signatures and test naming schemes aligned.
    - Do not introduce style modernizations or refactoring unless specifically
      instructed.

2.  **Resource & Handle Safety (RAII)**:
    - ALWAYS use `zx::handle` or specialized RAII classes (e.g., `zx::timer`,
      `zx::vmo`, `zx::channel`) instead of raw `zx_handle_t` where possible to
      prevent handle leaks.
    - If you must use raw `zx_handle_t` (e.g., when simulating a bad handle),
      ensure it is closed using `zx_handle_close(handle)` in all code paths,
      including teardown and failures.

3.  **Rights Restriction Pattern**:
    - To test rights restriction, use `zx_handle_replace` or the C++
      `zx::handle::replace` helper to duplicate or replace the handle with
      reduced rights.
    - Example:
      ```cpp
      zx::timer timer;
      ASSERT_OK(zx::timer::create(0, ZX_CLOCK_MONOTONIC, &timer));

      // Duplicate or replace the handle with all rights except WRITE
      zx::timer reduced_timer;
      ASSERT_OK(timer.replace(ZX_DEFAULT_TIMER_RIGHTS & ~ZX_RIGHT_WRITE, &reduced_timer));

      // Try to use the reduced handle and assert access denied
      EXPECT_EQ(reduced_timer.set(zx::time(), zx::duration(0)), ZX_ERR_ACCESS_DENIED);
      ```

4.  **Unified vs. Standalone Targets**:
    - Check the `BUILD.gn` file in `zircon/system/utest/core/`.
    - Determine if the target test runs in unified-only mode or is built into
      standalone packages.
    - If you add new test files, make sure to register them in the local
      `BUILD.gn` target's `sources` block.

### Build & Test Verification
Before handing off for review, compile and run the test locally:
1.  Configure build (if needed): `fx set bringup.x64 --with-base
    //bundles/bringup:tests` or the existing configuration.
2.  Build: `fx build`
3.  Run the standalone test package: `fx test core-<object_type>-test-package`
    (e.g. `fx test core-fifo-test-package`). Component tests run extremely fast
    (~15-30s) and avoid VM/paging timeouts that occur in emulator boots when KVM
    is unavailable.
4.  Verify the new tests executed: If necessary, run `ffx log dump | grep -E
    "(<ObjectType>Test|PASSED|FAILED)"` to confirm that your newly added tests
    were executed and passed.
5.  Format the code: Run `fx format-code` to ensure all modifications follow the
    project formatting standards.

---

## Phase 3: Code Review & Critique (Critic Subagent)

**Goal**: Validate the implementation against strict safety, reliability, and
conventions.

### Critique Checklist

1.  **Handle Leaks**: Verify that every handle created in the test is closed or
    automatically managed via RAII. Check edge-cases such as premature returns
    or assertions failing.
2.  **Incorrect Rights Assumptions**: Ensure the rights removed are the exact
    ones required for the syscall.
3.  **Flakiness / Races**: Check if any timers, events, or signals depend on
    precise wall-clock time or sleeping (`zx_nanosleep`), which causes
    flakiness. Prefer `zx_object_wait_async` or wait signals with robust
    deadlines.
4.  **GTest/ZXTest Semantics**: Ensure assertions use appropriate macros:
    - Use `ASSERT_OK(status)` for setup operations (if this fails, the test
      cannot continue).
    - Use `EXPECT_EQ(status, ZX_ERR_...)` for the actual validation assertions.
    - Do not mix raw boolean comparisons where explicit value checks are
      clearer.

---

## Phase 4: Orchestration Loop

The Main Agent coordinates the handoffs:
1.  **Assign**: Main Agent launches **Discovery** subagent.
2.  **Parse & Plan**: Main Agent reviews the gaps list and selects a subset to
    implement.
3.  **Code**: Main Agent launches **Coding** subagent to implement a gap.
4.  **Critique**: Main Agent launches **Critic** subagent on the diff.
5.  **Refine**: If Critic requests changes, Main Agent sends feedback back to
    the Coding subagent and repeats.
6.  **Final Verification**: Once the Critic approves and the tests compile and
    pass, the task is complete.
