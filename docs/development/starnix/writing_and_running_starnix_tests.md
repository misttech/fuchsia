# Writing and running Starnix tests

This guide provides instructions for running and writing automated tests for
Starnix.

## 1. Running existing tests {:#running-existing-tests}

The recommended way to verify features or reproduce bugs in Starnix is by
writing and running C++ syscall tests. These tests are located in
`//src/starnix/tests/syscalls/cpp/`.

To run the existing tests, configure a Fuchsia build that includes the Starnix
test targets:

1.  Configure the build:

    ```posix-terminal
    fx set workbench_eng.x64 \
        --with-test //src/starnix/tests/syscalls/cpp:starnix_syscalls_cpp_tests
    ```

2.  Build Fuchsia:

    ```posix-terminal
    fx build
    ```

3.  Start an emulator (or connect a Fuchsia device). For example, to start an
    emulator without a graphical interface:

    ```posix-terminal
    ffx emu start --headless
    ```

    For more options, see the [Fuchsia emulator instructions][ffx-emu].

4.  Run the tests:

    ```posix-terminal
    fx test starnix_syscalls_cpp_tests
    ```

## 2. Writing a new test {:#writing-a-new-test}

When adding a new syscall or kernel feature, add a corresponding test
in the `//src/starnix/tests/syscalls/cpp/` directory.

The following sections cover how to write these tests:

*   [Testing against the Linux kernel](#testing-against-the-linux-kernel):
    Understand how to cross-test the behavior against the host Linux kernel.
*   [Using test expectations](#using-test-expectations): Understand how to
    handle functionality that is not yet implemented in Starnix.
*   [Example: Creating a new test](#example-creating-a-new-test): Walk
    through an example of creating a new test file.

### Testing against the Linux kernel {:#testing-against-the-linux-kernel}

The purpose of the Starnix syscall test suite is to cross-test Starnix against
the Linux kernel. Because Starnix implements the Linux UAPI, the exact same
test binaries are compiled and run on the Linux host machine to verify the
understanding of Linux's behavior. These test binaries are then run on Fuchsia
to verify Starnix's implementation.

If a test fails when running against the host Linux kernel, it indicates that
the test itself is incorrect or the assumption about Linux's behavior is
wrong. You must fix the test to pass on Linux before using it to validate
Starnix.

To write a new test:

1.  **Develop the test and run it against the host Linux kernel.** Ensure your
    build is configured to include the tests:

    ```posix-terminal
    fx set workbench_eng.x64 \
        --with-test //src/starnix/tests/syscalls/cpp:tests
    ```

    Then, run the test on your Linux host. Host test target names are the same
    as the target test name, but prefixed with `starnix_` and suffixed with
    `_host`. For example, to run `hello_starnix_test`:

    ```posix-terminal
    fx test starnix_hello_starnix_test_host
    ```

    Note: GN's `syscall_test` template automatically creates a Linux target for
    each test. This Linux target is assigned the same target name as the Fuchsia
    test, but prefixed with `starnix_` and suffixed with `_host`.

    Iterate on the test until it passes 100% on the host Linux kernel.

2.  **Land the tests with expected failures.** If the syscall is not yet fully
    implemented in Starnix, the Starnix test target will fail. Instead of
    waiting for the syscall to be complete, you should add the expected
    failures using test expectations (see
    [Using test expectations](#using-test-expectations)) and land the test
    suite as a baseline.

3.  **Implement the functionality in Starnix.** With the baseline established
    as passing on Linux, begin implementing the syscall inside the Starnix
    kernel. For guidance on writing Starnix syscalls, see the
    [Starnix syscall rubric][starnix-rubric] and
    [Common coding patterns][starnix-patterns].

4.  **Run the test against Starnix.** With a Fuchsia device connected or an
    emulator running, run the test:

    ```posix-terminal
    fx test hello_starnix_test
    ```

    Iterate on the Starnix implementation until the tests pass. When they do,
    update the test expectations (see
    [Using test expectations](#using-test-expectations)) to remove the
    expected failures.

### Using test expectations {:#using-test-expectations}

Some syscalls that you test on the Linux host may not yet be fully implemented
in Starnix. When a test runs successfully on Linux but fails on Starnix, it will
turn the Fuchsia build red. Instead of deleting the test or waiting for the
syscall to be fully built, Starnix uses *test expectations* to explicitly record
which tests are *expected* to fail.

1.  **Locate the expectations file:** Expectations are defined in `.json5`
    files (for example:
    [`//src/starnix/tests/syscalls/cpp/expectations/syscalls_cpp_test.json5`](/src/starnix/tests/syscalls/cpp/expectations/syscalls_cpp_test.json5)).
2.  **Add a failing expectation:** Add the name of your failing test block
    to the `expect_failure` list.

    ```json5
    // expectations/syscalls_cpp_test.json5
    {
        actions: [
            {
                type: "expect_failure",
                matchers: [
                    // TODO(https://fxbug.dev/12345): Implement new sys_xyz
                    "HelloStarnixTest.FailingTest",
                ],
            },
        ],
    }
    ```

3.  **Land the test suite:** Commit and submit the test suite with the failing
    expectations. This establishes the Linux behavior as the baseline.
4.  **Implement the syscall:** In a subsequent CL, implement the syscall in
    Starnix.
5.  **Remove the expectation:** Once your Starnix implementation allows the test
    to pass, delete the entry from the `.json5` file. The test will now act as a
    regression guard going forward.

### Example: Creating a new test {:#example-creating-a-new-test}

For example, to create a new test file named `hello_starnix_test.cc` that tests
a syscall not yet implemented in Starnix:

1.  Create `//src/starnix/tests/syscalls/cpp/hello_starnix_test.cc`:

    ```cpp
    #include <gtest/gtest.h>

    namespace {

    TEST(HelloStarnixTest, Basic) {
      EXPECT_TRUE(true);
    }

    }  // namespace
    ```

2.  Add a failing expectation for the test in
    [`//src/starnix/tests/syscalls/cpp/expectations/syscalls_cpp_test.json5`](/src/starnix/tests/syscalls/cpp/expectations/syscalls_cpp_test.json5):

    ```json5
    // ... inside the file's `expect_failure` block:
    {
        type: "expect_failure",
        matchers: [
            // ... existing expectations ...
            "HelloStarnixTest.Basic",
        ],
    }
    ```

3.  Add `"hello_starnix_test"` to the `syscall_tests` list in
    [`//src/starnix/tests/syscalls/cpp/BUILD.gn`](/src/starnix/tests/syscalls/cpp/BUILD.gn):

    ```gn
    syscall_tests = [
      # ... other tests ...
      "hello_starnix_test",
      # ...
    ]
    ```

4.  Build the updated test package:

    ```posix-terminal
    fx build
    ```

5.  With a Fuchsia device or emulator running, execute the new test:

    ```posix-terminal
    fx test hello_starnix_test
    ```

Because an expectation was added in the `.json5` file, the test runner expects
the test to fail on Starnix. The build will succeed, and the test run will
report as passed. Once you implement the missing syscall in Starnix, you can
remove the expectation from the `.json5` file.

## What's next? {:#whats-next}

*   Learn more about [Starnix concepts][starnix-concepts].
*   Check out the [Starnix syscalls][starnix-syscalls] documentation.
*   Read about [Testing Starnix using Linux binaries][testing-starnix].

<!-- Reference links -->

[starnix-concepts]: /docs/concepts/components/v2/starnix.md
[starnix-syscalls]: /docs/concepts/starnix/syscalls.md
[starnix-rubric]: /docs/development/starnix/rubric_for_writing_starnix_syscalls.md
[starnix-patterns]: /docs/development/starnix/common_coding_patterns_in_starnix.md
[ffx-emu]: /docs/development/tools/ffx/workflows/start-the-fuchsia-emulator.md
[testing-fuchsia]: /docs/development/testing/testing.md
[testing-starnix]: /docs/development/starnix/common_coding_patterns_in_starnix.md#testing-starnix-using-linux-binaries
