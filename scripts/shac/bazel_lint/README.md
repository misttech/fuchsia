# Bazel Lint Checks for `shac`

This directory contains the pluggable Python-`ast`-based static analysis
framework and checks for Bazel files (e.g., `*.bazel`, `*.bzl`) invoked by
[`//scripts/shac/bazel.star`](../bazel.star).

## Handling False Positives or Requesting Exceptions

If you believe a Bazel lint check finding is a false positive or an exception is
needed:

1. Reach out to the Build team (see [`//build/OWNERS`](/build/OWNERS)).
2. File a bug in the Build > Bazel component:
   <https://issues.fuchsia.dev/issues/new?component=1477645>.
3. Add an entry referencing that bug to the check's allowlist following
   [`allowlists/README.md`](./allowlists/README.md) and get it reviewed by a
   member of [`//build/OWNERS`](/build/OWNERS).

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

To run a specific check across all files in the checkout (including `--fail-level
warning` so `shac` fails on `warning`-level findings emitted when
`enforcement = "unchanged_lines_warning_only"` is set, and inspecting the output
for any `notice` findings):

```sh
fx host-tool shac check --only <check_name> --all --fail-level warning
```

## Implementation Overview

* [`checks/`](./checks/README.md): Contains the individual Bazel lint check
  implementations (`<check_name>.py`) and optional per-check documentation
  (`<check_name>.md`). See [`checks/README.md`](./checks/README.md) for
  instructions on adding or modifying a check.
* [`allowlists/`](./allowlists/README.md): Contains optional per-check JSON
  allowlists. See [`allowlists/README.md`](./allowlists/README.md) for the JSON
  schema and guidelines.
* [`//scripts/shac/bazel.star`](../bazel.star): Registers one `shac` check per
  rule via a shared factory so findings are attributed per rule in Gerrit and
  `fx host-tool shac check --only <check_name>` works per rule.
* [`main.py`](./main.py): CLI entrypoint invoked by `bazel.star`. Validates that
  all input paths match a supported `FileKind`, loads the check's allowlist (if
  present), runs the selected check on each file, and emits findings as JSON.
* [`framework.py`](./framework.py): Parses each file once with Python's `ast`
  module into a `FileContext` and dispatches to the active `BazelCheck` subclass
  at four semantic hook points (`check_module`, `check_load`, `check_call`,
  `check_string`).
* [`allowlist.py`](./allowlist.py): Validates and matches per-rule allowlist
  JSON files under [`allowlists/`](./allowlists/README.md).

### Running Tests

When modifying the framework (`framework.py`, `allowlist.py`, `main.py`, or `BUILD.bazel` files), run:

* **Framework unit tests**:
  ```sh
  fx test --host //scripts/shac/tests:bazel_lint_test
  ```
  (Can also be run directly with `python3 scripts/shac/tests/bazel_lint_test.py` for fast iteration).

* **Hermetic Bazel test**:
  ```sh
  fx test --host @//scripts/shac/tests:bazel_test
  ```
  Runs `bazel_test.star` inside Bazel's hermetic sandbox to verify that all Python modules, rules, and test files are properly declared in `BUILD.bazel` files and staged in runfiles.

  If a test target is not recognized by `fx test`, register it first by running:
  ```sh
  fx add-host-test //scripts/shac/tests
  ```
