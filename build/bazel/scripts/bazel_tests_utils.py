# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Utility functions for working with Bazel test targets."""

import json
import os
import sys
import typing as T
from pathlib import Path

_SCRIPT_DIR = Path(__file__).parent
sys.path.append(str(_SCRIPT_DIR))
import build_utils
from build_utils import BazelLauncher, BazelPaths


def generate_tests_json(
    bazel_paths: BazelPaths,
    command_runner: T.Optional[build_utils.CommandRunner] = None,
) -> dict[str, T.Any]:
    """Generate a tests.json file corresponding to all Bazel host test targets

    Args:
        bazel_paths: The BazelPaths object to use for path resolution.
        command_runner: An optional CommandRunner instance.

    Returns:
        A list of tests.json objects describing each Bazel host_test() target
        reachable from the root_host_targets.
    """
    if not command_runner:
        command_runner = build_utils.CommandRunner()

    bazel_launcher = BazelLauncher(bazel_paths.launcher, runner=command_runner)
    starlark_input = _SCRIPT_DIR / "../starlark/FuchsiaHostTestInfo.cquery"

    ret = bazel_launcher.run_query(
        "cquery",
        [
            "--config=host",
            "--output=starlark",
            f"--starlark:file={starlark_input}",
            "deps(//build/bazel/host_tests)",
        ],
        False,
    )
    if ret.returncode != 0:
        raise RuntimeError(f"Failed to run bazel query: {ret.stderr}")

    def make_execroot_path_relative_to_ninja_build_dir(path: str) -> str:
        """Convert a path relative to the Bazel execroot to a path relative to the Ninja build directory."""
        return os.path.relpath(
            bazel_paths.execroot / path, bazel_paths.ninja_build_dir
        )

    tests_json: list[dict[str, T.Any]] = []

    for line in ret.stdout.splitlines():
        line = line.strip()
        if not line:
            continue

        # The line if a JSON-encoded object that follows the tests.json schema with
        # the following exceptions:
        #  - The 'bazel_execroot_path' and 'bazel_execroot_runtime_deps_path' fields
        #    are present instead of 'path' and 'runtime_deps_path', and they contain
        #    paths relative to the Bazel execroot instead of the Ninja build directory.
        cquery_test = json.loads(line)
        # LINT.IfChange(cquery_output_schema)
        tests_json.append(
            {
                "environments": [],
                "expects_ssh": False,
                "test": {
                    "name": cquery_test["name"],
                    "label": cquery_test["label"],
                    "path": make_execroot_path_relative_to_ninja_build_dir(
                        cquery_test["launcher_execroot_path"]
                    ),
                    "runtime_deps_path": make_execroot_path_relative_to_ninja_build_dir(
                        cquery_test["runtime_deps_json_execroot_path"]
                    ),
                    "os": cquery_test["os"],
                    "cpu": cquery_test["cpu"],
                },
            }
        )
        # LINT.ThenChange(//build/bazel/starlark/FuchsiaHostTestInfo.cquery:cquery_output_schema)

    return tests_json
