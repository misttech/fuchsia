# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Shared helpers for Bazel lint check Starlark tests.

Note: This file is named `*_test.star` because shac only exposes the
predeclared `testing` module to Starlark files ending in `_test.star`.
"""

# Mark the check's allowlist file as deleted in the virtual test root so shac
# detaches `scripts/shac/bazel_lint/allowlists/` from the real checkout and
# removes `<check_name>.json` if present on disk. This ensures unit tests run
# without any disk allowlist entries while also not creating a file on disk for
# checks with `supports_allowlist = False`.
_NO_ALLOWLIST = testing.file(
    action = "D",
    affected = False,
)

def run_lint_test(check, check_name, files):
    """Runs `check` on `files` without any disk allowlist for `check_name`.

    Per-check Starlark tests must always use this helper rather than calling
    `testing.run()` directly. It is usually wrapped by a private function at
    the top of `<check>_test.star`:

    ```starlark
    def _run_<check>_test(files):
        return run_lint_test(
            _bazel_<check>,
            "bazel_<check>",
            files,
        )
    ```

    Args:
      check: A `shac.check` object (for example, `_bazel_<check>`).
      check_name: Name of the shac check (for example, `"bazel_<check>"`).
      files: Dict mapping repository-relative file paths to file contents or
        `testing.file` objects.

    Returns:
      The result object returned by `testing.run()` (containing `.findings`
      and other test execution results).
    """
    allowlist_path = "scripts/shac/bazel_lint/allowlists/%s.json" % check_name
    if allowlist_path in files:
        fail("Per-check tests must not specify %s; allowlist matching is tested in bazel_test.star" % allowlist_path)
    test_files = {allowlist_path: _NO_ALLOWLIST}
    test_files.update(files)
    return testing.run(check, files = test_files)
