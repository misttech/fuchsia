//build/bazel/host_tests is the root of all host Bazel tests for the
Fuchsia build. These tests should all be runnable using the following
command:

```
fx bazel test --config=host //build/bazel/host_tests
```

To add another host test to the build, add its label to the `tests` attribute
of `//build/bazel/host_tests/BUILD.bazel. Use `test_suite()` to group related
tests (using `filegroup()` will *not* work).

Host tests must be wrapped or defined using the `host_test()` rule to be
visible to `fx test` and `botanist`. Such targets are also Bazel tests that
can be run directly with `fx bazel test`, but this will never be done in
production.

See https://fxbug.dev/349341932.
