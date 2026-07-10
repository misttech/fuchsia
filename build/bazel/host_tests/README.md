# Bazel Host Tests

`//build/bazel/host_tests` is the root of all host Bazel tests for the
Fuchsia build. Use the following command to run all Bazel host tests:

```sh
fx bazel test --config=host //build/bazel/host_tests
```

To add another host test to the build, add its label to the `tests` attribute
of `//build/bazel/host_tests/BUILD.bazel`. Use `test_suite()` to group related
tests (using `filegroup()` will *not* work).

Host tests must be wrapped or defined using the `host_test()` rule to be
visible to `fx test` and `botanist`. Such targets are also Bazel tests that
can be run directly with `fx bazel test`, but `bazel test` is not used in
infrastructure; instead, test are built with `bazel build` and then run as
binaries.

See https://fxbug.dev/349341932.
