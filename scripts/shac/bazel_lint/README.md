# Bazel Lint Checks for `shac`

This directory contains Python-based static analysis checks for Bazel files
(e.g., `*.bazel`, `*.bzl`) invoked by
[`//scripts/shac/bazel.star`](../bazel.star).

## Handling False Positives or Requesting Exceptions

If you believe a Bazel lint check finding is a false positive or an exception is
needed:

1. Reach out to the Build team (see [`//build/OWNERS`](/build/OWNERS)).
2. File a bug in the Build > Bazel component:
   <https://issues.fuchsia.dev/issues/new?component=1477645>.

## Running Checks Locally

To run the `shac` checks locally on modified files:
<!--
TODO(https://fxbug.dev/538640644): Replace with `fx lint` once it runs these shac checks.
-->
```sh
fx host-tool shac check
```

To run a specific check on all files in a directory:

```sh
fx host-tool shac check --only <check_name> path/to/dir/
```

To run a specific check across all files in the checkout:

```sh
fx host-tool shac check --only <check_name> --all
```

## Implementation Overview

* `checks/`: Contains the individual Bazel lint check implementations.
