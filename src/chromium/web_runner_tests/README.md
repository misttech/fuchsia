## `web_runner_tests`

Contains integration tests to ensure that Chromium is compatible with Fuchsia.

These actually test `web_engine` and are named this way for historical reasons.

## Build the test

```shell
$ fx set <product>.<arch> --with //src/chromium/web_runner_tests:tests
$ fx build
```

These tests require WebEngine to be available on the device. The `core`
product includes WebEngine in its cache packages; other products must include
it in their product configuration.

## Run the test

To run all the tests, use this fx invocation:

```shell
$ fx test web_runner_tests
```

To run individual test suites, use these fx invocations:

```shell
fx test web_runner_tests -t -- --gtest_filter="WebRunnerIntegrationTest.*"
```

For more information about the individual tests, see their respective file
comments.